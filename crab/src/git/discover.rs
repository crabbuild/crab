//! CLI-facing Git discovery adapter.
//!
//! Pure Git discovery lives in `crab-git`; this module keeps the existing
//! `CrabError`-shaped surface and Crab-specific `.crab/` path helpers.

use std::path::{Path, PathBuf};

use crate::core::error::Result;

/// Discover the `.git` directory from the current working directory.
pub fn discover_git_dir() -> Result<PathBuf> {
    Ok(crab_git::discover::discover_git_dir())
}

/// Discover the common Git directory from the current working directory.
pub fn discover_common_git_dir() -> Result<PathBuf> {
    let git_dir = discover_git_dir()?;
    Ok(resolve_common_dir(&git_dir))
}

/// Resolve the main worktree root directory.
pub fn resolve_main_worktree_root() -> Option<PathBuf> {
    crab_git::discover::main_worktree_root()
}

/// Resolve the current worktree root directory.
pub fn resolve_current_worktree_root() -> Option<PathBuf> {
    crab_git::discover::current_worktree_root()
}

/// Resolve the `.crab/` metadata directory.
pub fn resolve_crab_dir() -> Option<PathBuf> {
    resolve_main_worktree_root().map(|root| root.join(".crab"))
}

/// Resolve the common directory for a Git directory.
pub fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    crab_git::discover::resolve_common_dir(git_dir)
}

/// Like [`discover_git_dir`] but starts the search from an explicit directory.
#[expect(
    clippy::unnecessary_wraps,
    reason = "adapter preserves the old CrabError-shaped helper"
)]
pub(crate) fn discover_git_dir_from(start: &Path) -> Result<PathBuf> {
    Ok(crab_git::discover::discover_git_dir_from(start))
}

/// Like [`discover_common_git_dir`] but starts the search from an explicit directory.
pub(crate) fn discover_common_git_dir_from(start: &Path) -> Result<PathBuf> {
    let git_dir = discover_git_dir_from(start)?;
    Ok(resolve_common_dir(&git_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_returns_dot_git_when_no_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let result = discover_git_dir_from(tmp.path());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PathBuf::from(".git"));
    }

    #[test]
    fn discovers_standard_worktree_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::create_dir_all(git_dir.join("refs").join("heads")).unwrap();
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git_dir.join("refs").join("heads").join("main"),
            b"1111111111111111111111111111111111111111\n",
        )
        .unwrap();

        let result = discover_git_dir_from(tmp.path());
        assert!(result.is_ok());
        let discovered = result.unwrap();
        assert!(
            discovered.ends_with(".git"),
            "expected path ending in .git, got {discovered:?}"
        );
    }

    #[test]
    fn discovers_from_nested_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::create_dir_all(git_dir.join("refs").join("heads")).unwrap();
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git_dir.join("refs").join("heads").join("main"),
            b"1111111111111111111111111111111111111111\n",
        )
        .unwrap();

        let nested = tmp.path().join("src").join("deep").join("module");
        std::fs::create_dir_all(&nested).unwrap();

        let result = discover_git_dir_from(&nested);
        assert!(result.is_ok());
        let discovered = result.unwrap();
        assert!(
            discovered.ends_with(".git"),
            "expected path ending in .git, got {discovered:?}"
        );
    }
}
