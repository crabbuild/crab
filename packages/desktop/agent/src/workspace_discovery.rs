//! Remote workspace discovery.
//!
//! Scans a directory tree for git repositories (directories or files
//! named `.git`), respecting depth limits and exclude patterns. This
//! mirrors the logic in `electron/workspace-discovery.ts` so the
//! remote agent produces identical results to local discovery.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::fs;
use tracing::warn;

use crate::Result;

// ── Types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DiscoverReposParams {
    pub root_path: String,
    pub max_depth: u32,
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredRepo {
    pub absolute_path: String,
    pub relative_path: String,
    pub name: String,
}

// ── Public entry point ───────────────────────────────────────────

/// Discover git repositories under `root_path`, up to `max_depth`
/// levels deep, skipping directories whose names match any of the
/// `exclude_patterns`.
pub async fn discover_repos(params: DiscoverReposParams) -> Result<Value> {
    let root = PathBuf::from(&params.root_path);
    let mut repos = Vec::new();

    scan_directory(
        &root,
        &root,
        0,
        params.max_depth,
        &params.exclude_patterns,
        &mut repos,
    )
    .await;

    Ok(json!({ "repos": repos }))
}

// ── Recursive scanner ────────────────────────────────────────────

async fn scan_directory(
    current: &Path,
    root: &Path,
    depth: u32,
    max_depth: u32,
    exclude_patterns: &[String],
    results: &mut Vec<DiscoveredRepo>,
) {
    if depth > max_depth {
        return;
    }

    let mut entries = match fs::read_dir(current).await {
        Ok(entries) => entries,
        Err(e) => {
            // Permission denied or other read errors — skip gracefully.
            warn!(path = %current.display(), error = %e, "skipping inaccessible directory");
            return;
        }
    };

    // First pass: check if this directory contains a `.git` entry
    // (directory or file — git worktrees use a `.git` file).
    let mut children: Vec<(String, bool)> = Vec::new();
    let mut has_git = false;

    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => {
                warn!(path = %current.display(), error = %e, "error reading directory entry");
                continue;
            }
        };

        let name = entry.file_name().to_string_lossy().into_owned();

        if name == ".git" {
            // Accept both directory and file (worktree `.git` pointer files).
            has_git = true;
        }

        // Determine if this entry is a directory. `file_type()` may
        // fail on broken symlinks — treat those as non-directories.
        let is_dir = match entry.file_type().await {
            Ok(ft) => ft.is_dir(),
            Err(_) => false,
        };

        if is_dir && name != ".git" {
            children.push((name, is_dir));
        }
    }

    // If this directory is a git repo (and not the root itself),
    // record it and stop recursing — repos are leaf nodes.
    if has_git && current != root {
        let absolute_path = current.to_string_lossy().into_owned();
        let relative_path = current
            .strip_prefix(root)
            .unwrap_or(current)
            .to_string_lossy()
            .into_owned();
        let name = current
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        results.push(DiscoveredRepo {
            absolute_path,
            relative_path,
            name,
        });
        return;
    }

    // Recurse into child directories that aren't excluded.
    for (child_name, _) in children {
        if is_excluded(&child_name, exclude_patterns) {
            continue;
        }
        let child_path = current.join(&child_name);
        Box::pin(scan_directory(
            &child_path,
            root,
            depth + 1,
            max_depth,
            exclude_patterns,
            results,
        ))
        .await;
    }
}

/// Check if a directory name matches any of the exclude patterns.
/// Uses simple glob-style matching: a pattern without wildcards
/// matches the exact directory name; patterns with `*` or `?` use
/// basic glob semantics.
fn is_excluded(dir_name: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        if pattern_matches(pattern, dir_name) {
            return true;
        }
    }
    false
}

/// Minimal glob match supporting `*` (any sequence) and `?` (single
/// char). For the workspace discovery use case, patterns are almost
/// always plain names like `node_modules` or `target`, so exact match
/// is the fast path.
fn pattern_matches(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        return pattern == name;
    }
    glob_match(pattern.as_bytes(), name.as_bytes())
}

fn glob_match(pattern: &[u8], name: &[u8]) -> bool {
    let mut pi = 0;
    let mut ni = 0;
    let mut star_pi = usize::MAX;
    let mut star_ni = 0;

    while ni < name.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == name[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = pi;
            star_ni = ni;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ni += 1;
            ni = star_ni;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_pattern_matches_exact() {
        assert!(pattern_matches("node_modules", "node_modules"));
        assert!(!pattern_matches("node_modules", "node_module"));
        assert!(!pattern_matches("node_modules", "vendor"));
    }

    #[test]
    fn test_pattern_matches_glob() {
        assert!(pattern_matches("*.log", "app.log"));
        assert!(!pattern_matches("*.log", "app.txt"));
        assert!(pattern_matches("build*", "build_output"));
        assert!(pattern_matches("te?t", "test"));
        assert!(!pattern_matches("te?t", "teest"));
    }

    #[test]
    fn test_is_excluded() {
        let patterns = vec![
            "node_modules".to_owned(),
            "target".to_owned(),
            ".cache".to_owned(),
        ];
        assert!(is_excluded("node_modules", &patterns));
        assert!(is_excluded("target", &patterns));
        assert!(!is_excluded("src", &patterns));
    }

    #[tokio::test]
    async fn test_discover_repos_basic() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Create two repos
        std::fs::create_dir_all(root.join("repo-a/.git")).unwrap();
        std::fs::create_dir_all(root.join("repo-b/.git")).unwrap();
        // Create a non-repo directory
        std::fs::create_dir_all(root.join("docs")).unwrap();
        // Create a nested repo
        std::fs::create_dir_all(root.join("nested/repo-c/.git")).unwrap();

        let params = DiscoverReposParams {
            root_path: root.to_string_lossy().into_owned(),
            max_depth: 3,
            exclude_patterns: vec![],
        };

        let result = discover_repos(params).await.unwrap();
        let repos = result["repos"].as_array().unwrap();

        assert_eq!(repos.len(), 3);

        let names: Vec<&str> = repos.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"repo-a"));
        assert!(names.contains(&"repo-b"));
        assert!(names.contains(&"repo-c"));
    }

    #[tokio::test]
    async fn test_discover_repos_respects_max_depth() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Depth 1
        std::fs::create_dir_all(root.join("shallow/.git")).unwrap();
        // Depth 3 (too deep for max_depth=2)
        std::fs::create_dir_all(root.join("a/b/deep/.git")).unwrap();

        let params = DiscoverReposParams {
            root_path: root.to_string_lossy().into_owned(),
            max_depth: 2,
            exclude_patterns: vec![],
        };

        let result = discover_repos(params).await.unwrap();
        let repos = result["repos"].as_array().unwrap();

        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["name"].as_str().unwrap(), "shallow");
    }

    #[tokio::test]
    async fn test_discover_repos_respects_exclude_patterns() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        std::fs::create_dir_all(root.join("good-repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/hidden-repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("target/another/.git")).unwrap();

        let params = DiscoverReposParams {
            root_path: root.to_string_lossy().into_owned(),
            max_depth: 3,
            exclude_patterns: vec!["node_modules".to_owned(), "target".to_owned()],
        };

        let result = discover_repos(params).await.unwrap();
        let repos = result["repos"].as_array().unwrap();

        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["name"].as_str().unwrap(), "good-repo");
    }

    #[tokio::test]
    async fn test_discover_repos_git_file() {
        // Git worktrees use a `.git` file instead of a directory.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let repo_path = root.join("worktree-repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        std::fs::write(repo_path.join(".git"), "gitdir: /some/other/path").unwrap();

        let params = DiscoverReposParams {
            root_path: root.to_string_lossy().into_owned(),
            max_depth: 3,
            exclude_patterns: vec![],
        };

        let result = discover_repos(params).await.unwrap();
        let repos = result["repos"].as_array().unwrap();

        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["name"].as_str().unwrap(), "worktree-repo");
    }

    #[tokio::test]
    async fn test_discover_repos_does_not_recurse_into_found_repos() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // A repo with a nested repo inside — the nested one should NOT be found
        // because we stop recursing once we find a .git.
        std::fs::create_dir_all(root.join("parent-repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("parent-repo/sub/nested-repo/.git")).unwrap();

        let params = DiscoverReposParams {
            root_path: root.to_string_lossy().into_owned(),
            max_depth: 5,
            exclude_patterns: vec![],
        };

        let result = discover_repos(params).await.unwrap();
        let repos = result["repos"].as_array().unwrap();

        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["name"].as_str().unwrap(), "parent-repo");
    }

    #[tokio::test]
    async fn test_discover_repos_root_with_git_is_not_included() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // The root itself has .git — it should NOT be included in results
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("child-repo/.git")).unwrap();

        let params = DiscoverReposParams {
            root_path: root.to_string_lossy().into_owned(),
            max_depth: 3,
            exclude_patterns: vec![],
        };

        let result = discover_repos(params).await.unwrap();
        let repos = result["repos"].as_array().unwrap();

        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["name"].as_str().unwrap(), "child-repo");
    }
}
