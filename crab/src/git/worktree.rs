//! CLI-facing worktree layout adapter.
//!
//! Git discovery, version parsing, and porcelain decoding live in `crab-git`.
//! This module adds only Crab's per-worktree `.crab/` paths while preserving
//! the existing CLI-facing `WorktreeContext` shape.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::core::error::Result;

pub use crab_git::worktree::{
    GIT_2_39_WORKTREE_SURFACE, GitVersion, GitWorktreeRecord, LATEST_TRACKED_VERSION_GATED_OPTIONS,
    PorcelainField, REQUIRED_COMPATIBILITY_FLOOR, TRACKED_LATEST_MANUAL_VERSION,
    VersionGatedOption, WorktreeCommandSurface, normalize_identity_path,
};

pub fn installed_git_version() -> Result<Option<GitVersion>> {
    crab_git::worktree::installed_git_version().map_err(Into::into)
}

pub fn parse_worktree_list_porcelain(
    input: &[u8],
    nul_terminated: bool,
) -> Result<Vec<GitWorktreeRecord>> {
    crab_git::worktree::parse_worktree_list_porcelain(input, nul_terminated).map_err(Into::into)
}

pub fn linked_identity_map_from_current_repo() -> Result<HashMap<String, String>> {
    crab_git::worktree::linked_identity_map_from_current_repo().map_err(Into::into)
}

pub fn worktree_identity_for_path(path: &Path) -> Result<Option<String>> {
    crab_git::worktree::worktree_identity_for_path(path).map_err(Into::into)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeContext {
    pub current_worktree_root: PathBuf,
    pub main_worktree_root: PathBuf,
    pub common_git_dir: PathBuf,
    pub per_worktree_git_dir: PathBuf,
    pub shared_crab_dir: PathBuf,
    pub per_worktree_crab_dir: PathBuf,
    pub identity: String,
}

impl WorktreeContext {
    pub fn resolve() -> Result<Self> {
        crab_git::worktree::WorktreeContext::resolve()
            .map(Self::from)
            .map_err(Into::into)
    }

    pub fn resolve_from(start: &Path) -> Result<Self> {
        crab_git::worktree::WorktreeContext::resolve_from(start)
            .map(Self::from)
            .map_err(Into::into)
    }

    /// Resolves the Git worktree containing `start` without consulting Git environment overrides.
    pub fn resolve_from_path(start: &Path) -> Result<Self> {
        crab_git::worktree::WorktreeContext::resolve_from_path(start)
            .map(Self::from)
            .map_err(Into::into)
    }

    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.per_worktree_git_dir.join("index")
    }

    #[must_use]
    pub fn objects_dir(&self) -> PathBuf {
        self.common_git_dir.join("objects")
    }

    #[must_use]
    pub fn lfs_objects_dir(&self) -> PathBuf {
        self.common_git_dir.join("lfs").join("objects")
    }

    #[must_use]
    pub fn shared_staging_dir(&self) -> PathBuf {
        self.shared_crab_dir.join("staging")
    }
}

impl From<crab_git::worktree::WorktreeContext> for WorktreeContext {
    fn from(context: crab_git::worktree::WorktreeContext) -> Self {
        let shared_crab_dir = context.main_worktree_root.join(".crab");
        let per_worktree_crab_dir = shared_crab_dir.join("worktrees").join(&context.identity);

        Self {
            current_worktree_root: context.current_worktree_root,
            main_worktree_root: context.main_worktree_root,
            common_git_dir: context.common_git_dir,
            per_worktree_git_dir: context.per_worktree_git_dir,
            shared_crab_dir,
            per_worktree_crab_dir,
            identity: context.identity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_context_maps_to_shared_and_per_worktree_crab_paths() {
        let main = PathBuf::from("/repo");
        let context = crab_git::worktree::WorktreeContext {
            current_worktree_root: PathBuf::from("/linked"),
            main_worktree_root: main.clone(),
            common_git_dir: main.join(".git"),
            per_worktree_git_dir: main.join(".git/worktrees/linked-id"),
            identity: "linked-id".to_owned(),
        };

        let mapped = WorktreeContext::from(context);

        assert_eq!(mapped.shared_crab_dir, main.join(".crab"));
        assert_eq!(
            mapped.per_worktree_crab_dir,
            main.join(".crab/worktrees/linked-id")
        );
    }
}
