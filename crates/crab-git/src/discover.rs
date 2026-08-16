//! Git repository discovery helpers.
//!
//! This Module owns pure Git discovery and common-dir resolution. Crab-specific
//! paths such as `.crab/` stay in the CLI/product crate.

use std::path::{Path, PathBuf};

/// Discover the `.git` directory from the current working directory.
#[must_use]
pub fn discover_git_dir() -> PathBuf {
    discover_git_dir_from(Path::new("."))
}

/// Like [`discover_git_dir`] but starts from an explicit directory.
///
/// Discovery falls back to `.git` when the start path is not inside a Git
/// repository, matching the historical CLI helper used by `crab init`-style
/// flows.
#[must_use]
pub fn discover_git_dir_from(start: &Path) -> PathBuf {
    if let Ok(git_dir) = std::env::var("GIT_DIR")
        && !git_dir.is_empty()
    {
        return PathBuf::from(git_dir);
    }

    match gix_discover::upwards(start) {
        Ok((repo_path, _trust)) => {
            let (git_dir, _work_tree) = repo_path.into_repository_and_work_tree_directories();
            git_dir
        }
        Err(_) => PathBuf::from(".git"),
    }
}

/// Resolve the main worktree root directory from the current directory.
#[must_use]
pub fn main_worktree_root() -> Option<PathBuf> {
    main_worktree_root_from(Path::new("."))
}

/// Resolve the main worktree root directory from an explicit start path.
#[must_use]
pub fn main_worktree_root_from(start: &Path) -> Option<PathBuf> {
    let git_dir = discover_git_dir_from(start);
    let common_dir = resolve_common_dir(&git_dir);
    common_dir.parent().map(Path::to_path_buf)
}

/// Resolve the current worktree root directory from the current directory.
#[must_use]
pub fn current_worktree_root() -> Option<PathBuf> {
    match gix_discover::upwards(Path::new(".")) {
        Ok((repo_path, _trust)) => {
            let (_git_dir, work_tree) = repo_path.into_repository_and_work_tree_directories();
            work_tree.map(|p| p.to_path_buf())
        }
        Err(_) => None,
    }
}

/// Resolve the common directory for a Git directory.
///
/// For linked worktrees, `git_dir` contains a `commondir` file whose content is
/// a relative path to the shared `.git` directory. Normal repositories return
/// `git_dir` unchanged.
#[must_use]
pub fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    let commondir_file = git_dir.join("commondir");
    if let Ok(content) = std::fs::read_to_string(&commondir_file) {
        let relative = content.trim();
        if !relative.is_empty() {
            let resolved = git_dir.join(relative);
            if let Ok(canonical) = resolved.canonicalize() {
                return canonical;
            }
            return resolved;
        }
    }
    git_dir.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_returns_dot_git_when_no_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = discover_git_dir_from(tmp.path());
        assert_eq!(result, PathBuf::from(".git"));
    }

    #[test]
    fn discovers_standard_worktree_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        std::fs::create_dir_all(git_dir.join("refs").join("heads")).expect("refs");
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("head");
        std::fs::write(
            git_dir.join("refs").join("heads").join("main"),
            b"1111111111111111111111111111111111111111\n",
        )
        .expect("main");

        let discovered = discover_git_dir_from(tmp.path());
        assert!(
            discovered.ends_with(".git"),
            "expected path ending in .git, got {discovered:?}"
        );
    }

    #[test]
    fn discovers_from_nested_subdirectory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        std::fs::create_dir_all(git_dir.join("refs").join("heads")).expect("refs");
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("head");
        std::fs::write(
            git_dir.join("refs").join("heads").join("main"),
            b"1111111111111111111111111111111111111111\n",
        )
        .expect("main");

        let nested = tmp.path().join("src").join("deep").join("module");
        std::fs::create_dir_all(&nested).expect("nested");

        let discovered = discover_git_dir_from(&nested);
        assert!(
            discovered.ends_with(".git"),
            "expected path ending in .git, got {discovered:?}"
        );
    }

    #[test]
    fn common_dir_follows_linked_worktree_pointer() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let common = tmp.path().join(".git");
        let linked = common.join("worktrees").join("feature");
        std::fs::create_dir_all(&common).expect("common");
        std::fs::create_dir_all(&linked).expect("linked");
        std::fs::write(linked.join("commondir"), b"../..\n").expect("commondir");

        assert_eq!(
            resolve_common_dir(&linked),
            common.canonicalize().expect("canonical")
        );
    }

    #[test]
    fn main_worktree_root_from_follows_linked_worktree_common_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("main");
        let feature = tmp.path().join("feature");
        let common = main.join(".git");
        let linked = common.join("worktrees").join("feature");
        std::fs::create_dir_all(common.join("objects")).expect("objects");
        std::fs::create_dir_all(common.join("refs").join("heads")).expect("refs");
        std::fs::create_dir_all(&feature).expect("feature");
        std::fs::create_dir_all(&linked).expect("linked");
        std::fs::write(common.join("HEAD"), b"ref: refs/heads/main\n").expect("head");
        std::fs::write(
            common.join("refs").join("heads").join("main"),
            b"1111111111111111111111111111111111111111\n",
        )
        .expect("main");
        std::fs::write(
            feature.join(".git"),
            format!("gitdir: {}\n", linked.display()),
        )
        .expect("gitfile");
        std::fs::write(linked.join("commondir"), b"../..\n").expect("commondir");
        std::fs::write(
            linked.join("gitdir"),
            feature.join(".git").display().to_string(),
        )
        .expect("gitdir");
        std::fs::write(linked.join("HEAD"), b"ref: refs/heads/main\n").expect("linked head");

        assert_eq!(
            main_worktree_root_from(&feature).expect("root"),
            main.canonicalize().expect("canonical main")
        );
    }
}
