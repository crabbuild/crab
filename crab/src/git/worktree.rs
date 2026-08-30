//! CLI-facing worktree layout adapter.
//!
//! Git discovery, version parsing, and porcelain decoding live in `crab-git`.
//! This module adds only Crab's per-worktree `.crab/` paths while preserving
//! the existing CLI-facing `WorktreeContext` shape.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::core::error::{CrabError, Result};
use crab_types::pointer::Pointer;

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

/// Read Crab pointers for selected paths from `HEAD` and every local ref tip.
///
/// Returns an empty map outside an initialized worktree. Fails if a reachable
/// reference, commit, tree, or blob cannot be read exactly.
pub fn committed_pointers_for_paths(
    repo_root: &Path,
    paths: &[PathBuf],
) -> Result<HashMap<PathBuf, Vec<Pointer>>> {
    let dot_git = repo_root.join(".git");
    let dot_git_metadata = match std::fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HashMap::new());
        }
        Err(error) => return Err(CrabError::Io(error)),
    };
    if dot_git_metadata.is_dir() && !dot_git.join("HEAD").try_exists().map_err(CrabError::Io)? {
        return Ok(HashMap::new());
    }
    let repo = gix::open(repo_root)
        .map_err(|error| CrabError::Internal(format!("failed to open Git repository: {error}")))?;
    let mut tree_ids = HashSet::new();
    tree_ids.insert(
        repo.head_tree_id_or_empty()
            .map_err(|error| CrabError::Internal(format!("failed to resolve HEAD tree: {error}")))?
            .detach(),
    );
    let references = repo
        .references()
        .map_err(|error| CrabError::Internal(format!("failed to open Git references: {error}")))?;
    let references = references
        .all()
        .map_err(|error| CrabError::Internal(format!("failed to list Git references: {error}")))?
        .peeled()
        .map_err(|error| CrabError::Internal(format!("failed to peel Git references: {error}")))?;
    for reference in references {
        let reference = reference.map_err(|error| {
            CrabError::Internal(format!("failed to read Git reference: {error}"))
        })?;
        let object = reference.id().object().map_err(|error| {
            CrabError::Internal(format!(
                "failed to read Git reference {}: {error}",
                reference.name()
            ))
        })?;
        match object.kind {
            gix_object::Kind::Commit => {
                tree_ids.insert(
                    object
                        .into_commit()
                        .tree_id()
                        .map_err(|error| {
                            CrabError::Internal(format!(
                                "failed to read commit tree for {}: {error}",
                                reference.name()
                            ))
                        })?
                        .detach(),
                );
            }
            gix_object::Kind::Tree => {
                tree_ids.insert(object.id);
            }
            _ => {}
        }
    }
    let mut pointers = HashMap::with_capacity(paths.len());

    for tree_id in tree_ids {
        let tree = repo.find_tree(tree_id).map_err(|error| {
            CrabError::Internal(format!("failed to read committed Git tree: {error}"))
        })?;
        for path in paths {
            let Some(entry) = tree.lookup_entry_by_path(path).map_err(|error| {
                CrabError::Internal(format!(
                    "failed to read committed Git path {}: {error}",
                    path.display()
                ))
            })?
            else {
                continue;
            };
            if !entry.mode().is_blob() {
                continue;
            }
            let blob = repo.find_blob(entry.object_id()).map_err(|error| {
                CrabError::Internal(format!(
                    "failed to read committed Git blob for {}: {error}",
                    path.display()
                ))
            })?;
            if let Ok(pointer) = Pointer::parse(&blob.data) {
                let path_pointers = pointers.entry(path.clone()).or_insert_with(Vec::new);
                if !path_pointers.iter().any(|existing: &Pointer| {
                    existing.file_hash == pointer.file_hash && existing.size == pointer.size
                }) {
                    path_pointers.push(pointer);
                }
            }
        }
    }
    Ok(pointers)
}

/// Refresh Git index stat entries after Crab replaces worktree file contents.
pub(crate) fn refresh_index_stats(root: &Path, paths: &[PathBuf]) -> Result<usize> {
    use bstr::ByteSlice;

    if paths.is_empty() {
        return Ok(0);
    }
    let mut updates = HashMap::with_capacity(paths.len());
    for path in paths {
        let rel = path.strip_prefix(root).unwrap_or(path);
        let metadata = gix_index::fs::Metadata::from_path_no_follow(path).map_err(CrabError::Io)?;
        let stat = gix_index::entry::Stat::from_fs(&metadata)
            .map_err(|error| CrabError::Internal(format!("read worktree file stat: {error}")))?;
        updates.insert(index_path_bytes(rel), stat);
    }

    let index_path = WorktreeContext::resolve_from_path(root)?.index_path();
    let lock = gix_lock::File::acquire_to_update_resource(
        &index_path,
        gix_lock::acquire::Fail::Immediately,
        None,
    )
    .map_err(|error| CrabError::Internal(format!("lock Git index: {error}")))?;
    let mut index = gix_index::File::at(
        &index_path,
        gix_hash::Kind::Sha1,
        true,
        gix_index::decode::Options::default(),
    )
    .map_err(|error| CrabError::Internal(format!("read Git index: {error}")))?;
    let mut updated = 0usize;
    for (entry, entry_path) in index.entries_mut_with_paths() {
        if entry.stage() != gix_index::entry::Stage::Unconflicted {
            continue;
        }
        let Some(stat) = updates.get(entry_path.as_bytes()) else {
            continue;
        };
        entry.stat = *stat;
        updated += 1;
    }
    if updated == 0 {
        return Ok(0);
    }

    let mut writer = std::io::BufWriter::with_capacity(64 * 1024, lock);
    index
        .write_to(&mut writer, gix_index::write::Options::default())
        .map_err(|error| CrabError::Internal(format!("write Git index: {error}")))?;
    let lock = writer
        .into_inner()
        .map_err(|error| CrabError::Io(error.into_error()))?;
    lock.commit()
        .map_err(|error| CrabError::Internal(format!("commit Git index: {error}")))?;
    Ok(updated)
}

#[cfg(unix)]
fn index_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn index_path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
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
