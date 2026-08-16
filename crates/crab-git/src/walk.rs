//! Shared ref→commit→tree→blob traversal for GC and push pipeline.
//!
//! Both GC's reachable-set walk and the push pipeline's pointer enumeration
//! need to traverse from refs down to blobs. This module provides a single
//! implementation backed by `gix-odb` (for object lookup), `gix-traverse`
//! (for commit and tree walking), and `gix-object` (for blob parsing and
//! pointer detection).

use std::collections::HashSet;
use std::path::Path;

use gix_hash::ObjectId;
use gix_object::FindExt;
use tracing::{debug, warn};

use crab_types::pointer::Pointer;
use thiserror::Error;

/// Errors returned while traversing a local Git object database.
#[derive(Debug, Error)]
pub enum WalkError {
    #[error("git objects directory not found: {path}")]
    ObjectsDirectoryNotFound { path: String },
    #[error("reachable walk crossed missing local history at {oid}")]
    BeyondShallowBoundary { oid: String },
    #[error("{operation}")]
    Git {
        operation: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Result type for Git object walks.
pub type Result<T> = std::result::Result<T, WalkError>;

fn commit_walk_error(tip: &str, err: gix_traverse::commit::simple::Error) -> WalkError {
    match err {
        gix_traverse::commit::simple::Error::Find(source) => {
            debug!(
                error = %source,
                tip,
                "reachable walk crossed missing local history"
            );
            WalkError::BeyondShallowBoundary {
                oid: tip.to_owned(),
            }
        }
        other => WalkError::Git {
            operation: "commit walk failed".to_owned(),
            source: Box::new(other),
        },
    }
}

/// A blob that parses as a crab pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerBlob {
    /// The git object ID (SHA-1) of the blob.
    pub oid: [u8; 20],
    /// The Blake3 file hash from the pointer content.
    pub file_hash: [u8; 32],
    /// The original file size in bytes.
    pub size: u64,
}

/// The set of all reachable git objects discovered by [`walk_reachable`].
#[derive(Debug, Clone)]
pub struct ReachableSet {
    /// Reachable commit OIDs.
    pub commits: HashSet<[u8; 20]>,
    /// Reachable tree OIDs.
    pub trees: HashSet<[u8; 20]>,
    /// Reachable blob OIDs (includes pointer blobs).
    pub blobs: HashSet<[u8; 20]>,
    /// Blobs that parse as crab pointers.
    pub pointers: Vec<PointerBlob>,
}

impl ReachableSet {
    fn new() -> Self {
        Self {
            commits: HashSet::new(),
            trees: HashSet::new(),
            blobs: HashSet::new(),
            pointers: Vec::new(),
        }
    }
}

/// Convert a 20-byte SHA-1 slice to a fixed-size array.
fn oid_to_bytes(oid: &gix_hash::oid) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf.copy_from_slice(oid.as_bytes());
    buf
}

/// Walk all refs and return the set of reachable object IDs, plus any
/// blobs that parse as crab pointers.
///
/// Opens the git object database at `git_dir/objects` and uses
/// `gix-traverse` for commit/tree traversal. Each blob is tested against
/// [`Pointer::parse`] to detect pointer files.
///
/// # Errors
///
/// Returns [`WalkError::ObjectsDirectoryNotFound`] if the objects directory
/// is missing, or [`WalkError::Git`] if a referenced object is corrupt.
pub fn walk_reachable(git_dir: &Path, refs: &[(String, String)]) -> Result<ReachableSet> {
    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return Err(WalkError::ObjectsDirectoryNotFound {
            path: objects_dir.display().to_string(),
        });
    }

    let odb = gix_odb::at(&objects_dir).map_err(|source| WalkError::Git {
        operation: format!("failed to open git ODB at {}", objects_dir.display()),
        source: Box::new(source),
    })?;

    let mut result = ReachableSet::new();

    // Parse ref SHAs into ObjectIds and use them as tips for the commit walk.
    let tips: Vec<ObjectId> = refs
        .iter()
        .filter_map(|(ref_name, sha)| {
            match ObjectId::from_hex(sha.as_bytes()) {
                Ok(oid) => Some(oid),
                Err(e) => {
                    warn!(ref_name = %ref_name, sha = %sha, error = %e, "skipping ref with invalid SHA");
                    None
                }
            }
        })
        .collect();

    if tips.is_empty() {
        debug!("no valid ref tips, returning empty reachable set");
        return Ok(result);
    }
    let error_tip = refs
        .iter()
        .find_map(|(_, sha)| (!sha.is_empty()).then_some(sha.as_str()))
        .unwrap_or("(unknown)");

    // Walk commits using gix-traverse's Simple iterator.
    let commit_walk = gix_traverse::commit::Simple::new(tips, &odb);

    for info_result in commit_walk {
        let info = info_result.map_err(|e| commit_walk_error(error_tip, e))?;

        let commit_bytes = oid_to_bytes(&info.id);
        result.commits.insert(commit_bytes);

        // Get the tree OID from this commit.
        let tree_id = {
            let mut buf = Vec::new();
            let mut commit_iter =
                odb.find_commit_iter(&info.id, &mut buf)
                    .map_err(|source| WalkError::Git {
                        operation: format!("failed to read commit {}", info.id),
                        source: Box::new(source),
                    })?;
            commit_iter.tree_id().map_err(|source| WalkError::Git {
                operation: format!("failed to parse tree from commit {}", info.id),
                source: Box::new(source),
            })?
        };

        // Walk the tree breadth-first, collecting trees and blobs.
        walk_tree(&odb, &tree_id, &mut result)?;
    }

    debug!(
        commits = result.commits.len(),
        trees = result.trees.len(),
        blobs = result.blobs.len(),
        pointers = result.pointers.len(),
        "walk complete"
    );

    Ok(result)
}

/// Walk a single tree and all its descendants, collecting tree and blob OIDs.
fn walk_tree(
    odb: &impl gix_object::Find,
    tree_id: &gix_hash::oid,
    result: &mut ReachableSet,
) -> Result<()> {
    let tree_bytes = oid_to_bytes(tree_id);
    if !result.trees.insert(tree_bytes) {
        // Already visited this tree — skip to avoid redundant work.
        return Ok(());
    }

    let mut buf = Vec::new();
    let tree_iter = odb
        .find_tree_iter(tree_id, &mut buf)
        .map_err(|source| WalkError::Git {
            operation: format!("failed to read tree {tree_id}"),
            source: Box::new(source),
        })?;

    // Use gix-traverse breadth-first tree walk with a custom visitor.
    let mut visitor = ObjectCollector::new(result);
    let mut state = gix_traverse::tree::breadthfirst::State::default();

    gix_traverse::tree::breadthfirst(tree_iter, &mut state, odb, &mut visitor).map_err(
        |source| WalkError::Git {
            operation: format!("tree walk error at {tree_id}"),
            source: Box::new(source),
        },
    )?;

    // Now read each newly discovered blob to check for pointers.
    for blob_oid in &visitor.pending_blobs {
        check_blob_for_pointer(odb, blob_oid, result);
    }

    Ok(())
}

/// Check whether a blob is a crab pointer and record it if so.
fn check_blob_for_pointer(
    odb: &impl gix_object::Find,
    blob_id: &ObjectId,
    result: &mut ReachableSet,
) {
    let mut buf = Vec::new();
    let data = match odb.try_find(blob_id, &mut buf) {
        Ok(Some(data)) if data.kind == gix_object::Kind::Blob => data,
        Ok(Some(_)) => return, // not a blob — skip
        Ok(None) => {
            warn!(oid = %blob_id, "blob referenced in tree but not found in ODB");
            return;
        }
        Err(e) => {
            warn!(oid = %blob_id, error = %e, "failed to read blob");
            return;
        }
    };

    // Only attempt pointer parse on small blobs (pointers are ≤256 bytes).
    if data.data.len() <= crab_types::pointer::MAX_POINTER_SIZE
        && let Ok(ptr) = Pointer::parse(data.data)
    {
        result.pointers.push(PointerBlob {
            oid: oid_to_bytes(blob_id),
            file_hash: ptr.file_hash,
            size: ptr.size,
        });
    }
}

/// A [`gix_traverse::tree::Visit`] implementation that collects tree and blob
/// OIDs into a [`ReachableSet`], skipping already-visited trees.
struct ObjectCollector<'a> {
    result: &'a mut ReachableSet,
    /// Blob OIDs discovered during this tree walk, to be checked for pointers
    /// after the walk completes (we can't read blobs during the walk because
    /// the visitor borrows the ODB buffer).
    pending_blobs: Vec<ObjectId>,
}

impl<'a> ObjectCollector<'a> {
    fn new(result: &'a mut ReachableSet) -> Self {
        Self {
            result,
            pending_blobs: Vec::new(),
        }
    }
}

impl gix_traverse::tree::Visit for ObjectCollector<'_> {
    fn pop_front_tracked_path_and_set_current(&mut self) {}
    fn pop_back_tracked_path_and_set_current(&mut self) {}
    fn push_back_tracked_path_component(&mut self, _component: &gix_object::bstr::BStr) {}
    fn push_path_component(&mut self, _component: &gix_object::bstr::BStr) {}
    fn pop_path_component(&mut self) {}

    fn visit_tree(
        &mut self,
        entry: &gix_object::tree::EntryRef<'_>,
    ) -> std::ops::ControlFlow<(), bool> {
        let bytes = oid_to_bytes(entry.oid);
        if self.result.trees.insert(bytes) {
            // New tree — descend into it.
            std::ops::ControlFlow::Continue(true)
        } else {
            // Already visited — skip.
            std::ops::ControlFlow::Continue(false)
        }
    }

    fn visit_nontree(
        &mut self,
        entry: &gix_object::tree::EntryRef<'_>,
    ) -> std::ops::ControlFlow<(), bool> {
        if !entry.mode.is_blob_or_symlink() {
            return std::ops::ControlFlow::Continue(true);
        }

        let bytes = oid_to_bytes(entry.oid);
        if self.result.blobs.insert(bytes) && entry.mode.is_blob() {
            self.pending_blobs.push(entry.oid.to_owned());
        }
        std::ops::ControlFlow::Continue(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reachable_set_starts_empty() {
        let set = ReachableSet::new();
        assert!(set.commits.is_empty());
        assert!(set.trees.is_empty());
        assert!(set.blobs.is_empty());
        assert!(set.pointers.is_empty());
    }

    #[test]
    fn oid_to_bytes_round_trips() {
        let hex = b"aabbccddee00112233445566778899aabbccddee";
        let oid = ObjectId::from_hex(hex).unwrap();
        let bytes = oid_to_bytes(&oid);
        assert_eq!(bytes.len(), 20);
        // Convert back and compare.
        let oid2 = ObjectId::from_bytes_or_panic(&bytes);
        assert_eq!(oid, oid2);
    }

    #[test]
    fn walk_reachable_errors_on_missing_objects_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        // No objects/ subdirectory.

        let refs = vec![("refs/heads/main".into(), "a".repeat(40))];
        let err = walk_reachable(&git_dir, &refs).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not found"),
            "expected 'not found' in error, got: {msg}"
        );
    }

    #[test]
    fn walk_reachable_returns_empty_for_no_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        let objects_dir = git_dir.join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();

        let result = walk_reachable(&git_dir, &[]).unwrap();
        assert!(result.commits.is_empty());
        assert!(result.trees.is_empty());
        assert!(result.blobs.is_empty());
        assert!(result.pointers.is_empty());
    }

    #[test]
    fn walk_reachable_skips_invalid_sha_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        let objects_dir = git_dir.join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();

        let refs = vec![
            ("refs/heads/bad".into(), "not-a-valid-hex".into()),
            ("refs/heads/short".into(), "aabb".into()),
        ];
        // All refs are invalid, so we get an empty set (no error).
        let result = walk_reachable(&git_dir, &refs).unwrap();
        assert!(result.commits.is_empty());
    }

    #[test]
    fn walk_reachable_on_real_git_repo() {
        // Create a minimal git repo with `git init` + a commit.
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();
        let git_dir_path = repo_dir.join(".git");

        // Helper: run git in the temp repo, ignoring any inherited GIT_DIR.
        macro_rules! git {
            ($($arg:expr),+ $(,)?) => {
                std::process::Command::new("git")
                    .args([$($arg),+])
                    .current_dir(repo_dir)
                    .env("GIT_DIR", &git_dir_path)
            };
        }

        // git init
        let status = git!("init", "--initial-branch=main")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let Ok(s) = status else {
            eprintln!("skipping test: git not available");
            return;
        };
        if !s.success() {
            eprintln!("skipping test: git init failed");
            return;
        }

        let _ = git!("config", "user.email", "test@test.com").status();
        let _ = git!("config", "user.name", "Test").status();

        std::fs::write(repo_dir.join("hello.txt"), b"hello world\n").unwrap();
        let _ = git!("add", "hello.txt").status();
        let _ = git!("commit", "-m", "initial")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let output = git!("rev-parse", "HEAD").output().unwrap();
        let head_sha = String::from_utf8(output.stdout).unwrap().trim().to_string();

        if head_sha.len() != 40 {
            eprintln!("skipping test: could not get HEAD sha");
            return;
        }

        let refs = vec![("refs/heads/main".into(), head_sha)];
        let result = walk_reachable(&git_dir_path, &refs).unwrap();

        assert_eq!(result.commits.len(), 1, "expected 1 commit");
        assert!(!result.trees.is_empty(), "expected at least 1 tree");
        assert!(!result.blobs.is_empty(), "expected at least 1 blob");
        assert!(result.pointers.is_empty(), "expected no pointers");
    }

    #[test]
    fn walk_reachable_detects_pointer_blob() {
        // Create a git repo with a blob that is a valid crab pointer.
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();
        let git_dir_path = repo_dir.join(".git");

        macro_rules! git {
            ($($arg:expr),+ $(,)?) => {
                std::process::Command::new("git")
                    .args([$($arg),+])
                    .current_dir(repo_dir)
                    .env("GIT_DIR", &git_dir_path)
            };
        }

        let status = git!("init", "--initial-branch=main")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let Ok(s) = status else {
            eprintln!("skipping test: git not available");
            return;
        };
        if !s.success() {
            eprintln!("skipping test: git init failed");
            return;
        }

        let _ = git!("config", "user.email", "test@test.com").status();
        let _ = git!("config", "user.name", "Test").status();

        // Write a valid crab pointer as a file.
        let pointer_content = format!(
            "version https://crab.dev/spec/v1\nfile-hash {}\nsize 42\n",
            "ab".repeat(32)
        );
        std::fs::write(repo_dir.join("data.bin"), pointer_content.as_bytes()).unwrap();

        let _ = git!("add", "data.bin").status();
        let _ = git!("commit", "-m", "add pointer")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let output = git!("rev-parse", "HEAD").output().unwrap();
        let head_sha = String::from_utf8(output.stdout).unwrap().trim().to_string();

        if head_sha.len() != 40 {
            eprintln!("skipping test: could not get HEAD sha");
            return;
        }

        let refs = vec![("refs/heads/main".into(), head_sha)];
        let result = walk_reachable(&git_dir_path, &refs).unwrap();

        assert_eq!(result.pointers.len(), 1, "expected 1 pointer blob");
        assert_eq!(result.pointers[0].size, 42);
    }

    #[test]
    fn walk_reachable_skips_submodule_gitlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();
        let git_dir_path = repo_dir.join(".git");
        let gitlink_oid = "52b7efa603f1b809167b528b8bbaa467e36fdc02";

        macro_rules! git {
            ($($arg:expr),+ $(,)?) => {
                std::process::Command::new("git")
                    .args([$($arg),+])
                    .current_dir(repo_dir)
                    .env("GIT_DIR", &git_dir_path)
            };
        }

        let Ok(s) = git!("init", "--initial-branch=main")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        else {
            eprintln!("skipping test: git not available");
            return;
        };
        if !s.success() {
            eprintln!("skipping test: git init failed");
            return;
        }

        let _ = git!("config", "user.email", "test@test.com").status();
        let _ = git!("config", "user.name", "Test").status();
        let _ = git!(
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            gitlink_oid,
            "vendor/lib"
        )
        .status();
        let _ = git!("commit", "-m", "add submodule")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let output = git!("rev-parse", "HEAD").output().unwrap();
        let head_sha = String::from_utf8(output.stdout).unwrap().trim().to_string();
        if head_sha.len() != 40 {
            eprintln!("skipping test: could not get HEAD sha");
            return;
        }

        let refs = vec![("refs/heads/main".into(), head_sha)];
        let result = walk_reachable(&git_dir_path, &refs).unwrap();
        let gitlink_bytes = oid_to_bytes(&ObjectId::from_hex(gitlink_oid.as_bytes()).unwrap());

        assert!(
            !result.blobs.contains(&gitlink_bytes),
            "submodule gitlinks are not superproject blobs"
        );
        assert!(result.pointers.is_empty());
    }

    /// GC needs to walk the reachable set without crossing into
    /// orphaned commits. Build a fixture with two commits — one
    /// reachable from `refs/heads/main`, one orphaned (created via
    /// `git commit-tree` and never referenced). The walk from
    /// `refs/heads/main` should include the reachable commit's
    /// OID and *not* the orphan.
    #[test]
    fn gc_walks_reachable_set_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();
        let git_dir_path = repo_dir.join(".git");

        macro_rules! git {
            ($($arg:expr),+ $(,)?) => {
                std::process::Command::new("git")
                    .args([$($arg),+])
                    .current_dir(repo_dir)
                    .env("GIT_DIR", &git_dir_path)
            };
        }

        let Ok(s) = git!("init", "--initial-branch=main")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        else {
            eprintln!("skipping: git not available");
            return;
        };
        if !s.success() {
            eprintln!("skipping: git init failed");
            return;
        }

        let _ = git!("config", "user.email", "test@test.com").status();
        let _ = git!("config", "user.name", "Test").status();

        // Reachable commit: commit on main.
        std::fs::write(repo_dir.join("reachable.txt"), b"reach\n").unwrap();
        let _ = git!("add", "reachable.txt").status();
        let _ = git!("commit", "-m", "reachable")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let out = git!("rev-parse", "HEAD").output().unwrap();
        let reachable_sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
        if reachable_sha.len() != 40 {
            eprintln!("skipping: could not read HEAD sha");
            return;
        }

        // Orphan: a dangling commit with no ref pointing at it. Build
        // it via `hash-object` + `mktree` + `commit-tree`. Safer than
        // messing with `update-ref` because we don't want a ref
        // accidentally created.
        let mut hash_child = std::process::Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(repo_dir)
            .env("GIT_DIR", &git_dir_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        hash_child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"orphan\n")
            .unwrap();
        let hash_out = hash_child.wait_with_output().unwrap();
        let blob_sha = String::from_utf8(hash_out.stdout)
            .unwrap()
            .trim()
            .to_string();
        if blob_sha.len() != 40 {
            eprintln!("skipping: could not hash orphan blob");
            return;
        }

        let mut mktree = std::process::Command::new("git")
            .args(["mktree"])
            .current_dir(repo_dir)
            .env("GIT_DIR", &git_dir_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        mktree
            .stdin
            .as_mut()
            .unwrap()
            .write_all(format!("100644 blob {blob_sha}\torphan.txt\n").as_bytes())
            .unwrap();
        let tree_out = mktree.wait_with_output().unwrap();
        let orphan_tree_sha = String::from_utf8(tree_out.stdout)
            .unwrap()
            .trim()
            .to_string();
        if orphan_tree_sha.len() != 40 {
            eprintln!("skipping: could not mktree orphan");
            return;
        }

        let commit_out = git!("commit-tree", &orphan_tree_sha, "-m", "orphan commit")
            .output()
            .unwrap();
        let orphan_commit_sha = String::from_utf8(commit_out.stdout)
            .unwrap()
            .trim()
            .to_string();
        if orphan_commit_sha.len() != 40 {
            eprintln!("skipping: could not create orphan commit");
            return;
        }

        // Walk from main only. The reachable commit must be present;
        // the orphan commit must be absent.
        let refs = vec![("refs/heads/main".into(), reachable_sha.clone())];
        let result = walk_reachable(&git_dir_path, &refs).unwrap();

        let reachable_bytes = {
            let oid = ObjectId::from_hex(reachable_sha.as_bytes()).unwrap();
            oid_to_bytes(&oid)
        };
        let orphan_bytes = {
            let oid = ObjectId::from_hex(orphan_commit_sha.as_bytes()).unwrap();
            oid_to_bytes(&oid)
        };

        assert!(
            result.commits.contains(&reachable_bytes),
            "reachable commit should be in the walk's commit set"
        );
        assert!(
            !result.commits.contains(&orphan_bytes),
            "orphan commit must NOT be in the walk's commit set"
        );
    }
}
