//! Shared ref→commit→tree→blob traversal for GC and push pipeline.
//!
//! Both GC's reachable-set walk and the push pipeline's pointer enumeration
//! need to traverse from refs down to blobs. This module provides a single
//! implementation backed by `gix-odb` (for object lookup), `gix-traverse`
//! (for commit and tree walking), and `gix-object` (for blob parsing and
//! pointer detection).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use gix_hash::ObjectId;
use gix_object::{Find, FindExt, FindHeader};
use tracing::{debug, warn};

use crab_types::pointer::Pointer;
use thiserror::Error;

mod scan;
pub use scan::{PointerScan, PointerScanLimits, scan_pointers};

/// Errors returned while traversing a local Git object database.
#[derive(Debug, Error)]
pub enum WalkError {
    #[error("git objects directory not found: {path}")]
    ObjectsDirectoryNotFound { path: String },
    #[error("reachable walk crossed missing local history at {oid}")]
    BeyondShallowBoundary { oid: String },
    #[error("reachable walk exceeded {maximum} objects (observed at least {actual})")]
    LimitExceeded { actual: usize, maximum: usize },
    #[error("Git pointer scan cancelled")]
    Cancelled,
    #[error("Git pointer scan exceeded {maximum} object lookups")]
    LookupLimitExceeded { maximum: usize },
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
        gix_traverse::commit::simple::Error::Find(
            gix_object::find::existing_iter::Error::NotFound { .. },
        ) => {
            debug!(tip, "reachable walk crossed missing local history");
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
    /// Reachable annotated-tag OIDs.
    pub tags: HashSet<[u8; 20]>,
    /// Blobs that parse as crab pointers.
    pub pointers: Vec<PointerBlob>,
}

impl ReachableSet {
    fn new() -> Self {
        Self {
            commits: HashSet::new(),
            trees: HashSet::new(),
            blobs: HashSet::new(),
            tags: HashSet::new(),
            pointers: Vec::new(),
        }
    }

    /// Return the number of distinct Git objects in this closure.
    #[must_use]
    pub fn object_count(&self) -> usize {
        self.commits
            .len()
            .saturating_add(self.trees.len())
            .saturating_add(self.blobs.len())
            .saturating_add(self.tags.len())
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
    walk_reachable_with_limit(git_dir, refs, None)
}

/// Walk reachable objects with a fail-closed distinct-object bound.
pub fn walk_reachable_bounded(
    git_dir: &Path,
    refs: &[(String, String)],
    maximum: usize,
) -> Result<ReachableSet> {
    walk_reachable_with_limit(git_dir, refs, Some(maximum))
}

fn walk_reachable_with_limit(
    git_dir: &Path,
    refs: &[(String, String)],
    maximum: Option<usize>,
) -> Result<ReachableSet> {
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

    walk_commits(&odb, refs, maximum)
}

fn walk_commits(
    odb: &(impl Find + FindHeader),
    refs: &[(String, String)],
    maximum: Option<usize>,
) -> Result<ReachableSet> {
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
    let commit_walk = gix_traverse::commit::Simple::new(tips, odb);

    for info_result in commit_walk {
        let info = info_result.map_err(|e| commit_walk_error(error_tip, e))?;

        let commit_bytes = oid_to_bytes(&info.id);
        result.commits.insert(commit_bytes);
        check_reachable_limit(&result, maximum)?;

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
        walk_tree(odb, &tree_id, &mut result, maximum)?;
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

/// Walk each ref independently, preserving the object closure rooted at that
/// ref. Annotated tags are peeled only for commit traversal, but every tag in
/// the tag chain is retained in the corresponding closure.
pub fn walk_reachable_by_ref(
    git_dir: &Path,
    refs: &[(String, String)],
    peeled_refs: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, ReachableSet>> {
    walk_reachable_by_ref_with_limit(git_dir, refs, peeled_refs, None)
}

/// Walk each ref independently with a fail-closed distinct-object bound.
///
/// The bound applies to the union of all ref closures, so branches that share
/// history do not consume the same object budget repeatedly.
pub fn walk_reachable_by_ref_bounded(
    git_dir: &Path,
    refs: &[(String, String)],
    peeled_refs: &BTreeMap<String, String>,
    maximum: usize,
) -> Result<BTreeMap<String, ReachableSet>> {
    walk_reachable_by_ref_with_limit(git_dir, refs, peeled_refs, Some(maximum))
}

fn walk_reachable_by_ref_with_limit(
    git_dir: &Path,
    refs: &[(String, String)],
    peeled_refs: &BTreeMap<String, String>,
    maximum: Option<usize>,
) -> Result<BTreeMap<String, ReachableSet>> {
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

    let mut closures = BTreeMap::new();
    visit_reachable_by_ref(&odb, refs, peeled_refs, maximum, |name, closure| {
        closures.insert(name.to_owned(), closure);
        Ok(())
    })?;
    Ok(closures)
}

fn visit_reachable_by_ref(
    odb: &(impl Find + FindHeader),
    refs: &[(String, String)],
    peeled_refs: &BTreeMap<String, String>,
    maximum: Option<usize>,
    mut visit: impl FnMut(&str, ReachableSet) -> Result<()>,
) -> Result<()> {
    // A push client may not have tips concurrently published by another
    // writer. Prove every root is locally available before walking any large
    // closure so an incomplete ODB fails in O(refs), not O(repository size).
    for (_, oid) in refs {
        let root = ObjectId::from_hex(oid.as_bytes()).map_err(|source| WalkError::Git {
            operation: format!("invalid ref object {oid}"),
            source: Box::new(source),
        })?;
        if odb
            .try_header(&root)
            .map_err(|source| WalkError::Git {
                operation: format!("failed to read ref object {root}"),
                source,
            })?
            .is_none()
        {
            return Err(WalkError::BeyondShallowBoundary { oid: oid.clone() });
        }
    }

    let mut seen_objects = maximum.map(|_| HashSet::new());
    for (name, oid) in refs {
        let mut closure = ReachableSet::new();
        let object = ObjectId::from_hex(oid.as_bytes()).map_err(|source| WalkError::Git {
            operation: format!("invalid ref object {oid}"),
            source: Box::new(source),
        })?;
        let annotated = odb
            .try_header(&object)
            .map_err(|source| WalkError::Git {
                operation: format!("failed to read ref object {object}"),
                source,
            })?
            .is_some_and(|header| header.kind == gix_object::Kind::Tag);
        let traversal_tip = if annotated || peeled_refs.contains_key(name) {
            collect_annotated_tag_chain(odb, oid, &mut closure, maximum)?
                .to_hex()
                .to_string()
        } else {
            oid.clone()
        };
        check_reachable_limit(&closure, maximum)?;
        let root =
            ObjectId::from_hex(traversal_tip.as_bytes()).map_err(|source| WalkError::Git {
                operation: format!("invalid ref object {traversal_tip}"),
                source: Box::new(source),
            })?;
        let data = odb
            .try_header(&root)
            .map_err(|source| WalkError::Git {
                operation: format!("failed to read ref object {root}"),
                source,
            })?
            .ok_or_else(|| WalkError::BeyondShallowBoundary {
                oid: traversal_tip.clone(),
            })?;
        match data.kind {
            gix_object::Kind::Commit => {
                merge_reachable(
                    &mut closure,
                    walk_commits(odb, &[(name.clone(), traversal_tip)], maximum)?,
                );
            }
            gix_object::Kind::Tree => walk_tree(odb, &root, &mut closure, maximum)?,
            gix_object::Kind::Blob => {
                closure.blobs.insert(oid_to_bytes(&root));
                check_blob_for_pointer(odb, &root, &mut closure)?;
                check_reachable_limit(&closure, maximum)?;
            }
            gix_object::Kind::Tag => {
                return Err(WalkError::Git {
                    operation: format!("annotated tag {root} did not resolve"),
                    source: Box::new(std::io::Error::other("tag chain did not resolve")),
                });
            }
        }
        if let (Some(seen_objects), Some(maximum)) = (seen_objects.as_mut(), maximum) {
            record_reachable_objects(seen_objects, &closure);
            if seen_objects.len() > maximum {
                return Err(WalkError::LimitExceeded {
                    actual: seen_objects.len(),
                    maximum,
                });
            }
        }
        visit(name, closure)?;
    }
    Ok(())
}

fn record_reachable_objects(seen: &mut HashSet<[u8; 20]>, closure: &ReachableSet) {
    seen.extend(closure.commits.iter().copied());
    seen.extend(closure.trees.iter().copied());
    seen.extend(closure.blobs.iter().copied());
    seen.extend(closure.tags.iter().copied());
}

fn merge_reachable(target: &mut ReachableSet, source: ReachableSet) {
    target.commits.extend(source.commits);
    target.trees.extend(source.trees);
    target.blobs.extend(source.blobs);
    target.tags.extend(source.tags);
    target.pointers.extend(source.pointers);
}

fn collect_annotated_tag_chain(
    odb: &impl Find,
    start: &str,
    result: &mut ReachableSet,
    maximum: Option<usize>,
) -> Result<ObjectId> {
    let mut current = ObjectId::from_hex(start.as_bytes()).map_err(|source| WalkError::Git {
        operation: format!("invalid annotated tag object {start}"),
        source: Box::new(source),
    })?;

    for _ in 0..32 {
        let mut buf = Vec::new();
        let Some(data) = odb
            .try_find(&current, &mut buf)
            .map_err(|source| WalkError::Git {
                operation: format!("failed to read annotated tag {current}"),
                source,
            })?
        else {
            return Err(WalkError::Git {
                operation: format!("annotated tag {current} is missing"),
                source: Box::new(std::io::Error::other("missing object")),
            });
        };
        if data.kind != gix_object::Kind::Tag {
            return Ok(current);
        }
        result.tags.insert(oid_to_bytes(&current));
        check_reachable_limit(result, maximum)?;
        let tag = gix_object::TagRef::from_bytes(data.data, data.hash_kind).map_err(|source| {
            WalkError::Git {
                operation: format!("failed to parse annotated tag {current}"),
                source: Box::new(source),
            }
        })?;
        let target = tag.target();
        if tag.target_kind == gix_object::Kind::Tag {
            current = target;
            continue;
        }
        return Ok(target);
    }

    Err(WalkError::Git {
        operation: format!("annotated tag chain from {start} exceeds the limit"),
        source: Box::new(std::io::Error::other("tag recursion limit")),
    })
}

/// Walk a single tree and all its descendants, collecting tree and blob OIDs.
fn walk_tree(
    odb: &(impl Find + FindHeader),
    tree_id: &gix_hash::oid,
    result: &mut ReachableSet,
    maximum: Option<usize>,
) -> Result<()> {
    let tree_bytes = oid_to_bytes(tree_id);
    if !result.trees.insert(tree_bytes) {
        // Already visited this tree — skip to avoid redundant work.
        return Ok(());
    }
    check_reachable_limit(result, maximum)?;

    let mut buf = Vec::new();
    let tree_iter = odb
        .find_tree_iter(tree_id, &mut buf)
        .map_err(|source| WalkError::Git {
            operation: format!("failed to read tree {tree_id}"),
            source: Box::new(source),
        })?;

    // Use gix-traverse breadth-first tree walk with a custom visitor.
    let mut visitor = ObjectCollector::new(result, maximum);
    let mut state = gix_traverse::tree::breadthfirst::State::default();

    let traversal = gix_traverse::tree::breadthfirst(tree_iter, &mut state, odb, &mut visitor);

    if visitor.overflowed
        && let Some(maximum) = maximum
    {
        return Err(limit_error(visitor.result, maximum));
    }

    traversal.map_err(|source| WalkError::Git {
        operation: format!("tree walk error at {tree_id}"),
        source: Box::new(source),
    })?;

    // Now read each newly discovered blob to check for pointers.
    for blob_oid in &visitor.pending_blobs {
        check_blob_for_pointer(odb, blob_oid, result)?;
    }

    Ok(())
}

/// Check whether a blob is a crab pointer and record it if so.
fn check_blob_for_pointer(
    odb: &(impl Find + FindHeader),
    blob_id: &ObjectId,
    result: &mut ReachableSet,
) -> Result<()> {
    // Ordinary file bodies need not be decoded to classify pointer candidates.
    // Missing or unreadable headers are incomplete history, never non-pointers.
    let header = odb
        .try_header(blob_id)
        .map_err(|source| WalkError::Git {
            operation: format!("failed to read blob header {blob_id}"),
            source,
        })?
        .ok_or_else(|| WalkError::BeyondShallowBoundary {
            oid: blob_id.to_string(),
        })?;
    if header.kind != gix_object::Kind::Blob {
        return Err(WalkError::Git {
            operation: format!("tree references non-blob object {blob_id} as a blob"),
            source: Box::new(std::io::Error::other("Git object kind mismatch")),
        });
    }
    if header.size > crab_types::pointer::MAX_POINTER_SIZE as u64 {
        return Ok(());
    }
    let mut buf = Vec::new();
    let data = odb
        .try_find(blob_id, &mut buf)
        .map_err(|source| WalkError::Git {
            operation: format!("failed to read pointer candidate {blob_id}"),
            source,
        })?
        .ok_or_else(|| WalkError::BeyondShallowBoundary {
            oid: blob_id.to_string(),
        })?;
    data.verify_checksum(blob_id)
        .map_err(|source| WalkError::Git {
            operation: format!("pointer candidate checksum differs from {blob_id}"),
            source: Box::new(source),
        })?;

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
    Ok(())
}

/// A [`gix_traverse::tree::Visit`] implementation that collects tree and blob
/// OIDs into a [`ReachableSet`], skipping already-visited trees.
struct ObjectCollector<'a> {
    result: &'a mut ReachableSet,
    /// Blob OIDs discovered during this tree walk, to be checked for pointers
    /// after the walk completes (we can't read blobs during the walk because
    /// the visitor borrows the ODB buffer).
    pending_blobs: Vec<ObjectId>,
    maximum: Option<usize>,
    overflowed: bool,
}

impl<'a> ObjectCollector<'a> {
    fn new(result: &'a mut ReachableSet, maximum: Option<usize>) -> Self {
        Self {
            result,
            pending_blobs: Vec::new(),
            maximum,
            overflowed: false,
        }
    }

    fn exceeds_limit(&self) -> bool {
        self.maximum
            .is_some_and(|maximum| self.result.object_count() > maximum)
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
            if self.exceeds_limit() {
                self.overflowed = true;
                return std::ops::ControlFlow::Break(());
            }
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
        if entry.mode.is_commit() {
            // A gitlink records a submodule commit, but that object belongs to
            // another repository and is not part of this superproject's closure.
            return std::ops::ControlFlow::Continue(false);
        }

        let bytes = oid_to_bytes(entry.oid);
        if self.result.blobs.insert(bytes) && entry.mode.is_blob() {
            self.pending_blobs.push(entry.oid.to_owned());
        }
        if self.exceeds_limit() {
            self.overflowed = true;
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(true)
    }
}

fn check_reachable_limit(result: &ReachableSet, maximum: Option<usize>) -> Result<()> {
    if let Some(maximum) = maximum
        && result.object_count() > maximum
    {
        return Err(limit_error(result, maximum));
    }
    Ok(())
}

fn limit_error(result: &ReachableSet, maximum: usize) -> WalkError {
    WalkError::LimitExceeded {
        actual: result.object_count(),
        maximum,
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
        assert!(set.tags.is_empty());
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

        let refs = vec![("refs/heads/main".into(), head_sha.clone())];
        let result = walk_reachable(&git_dir_path, &refs).unwrap();

        assert_eq!(result.commits.len(), 1, "expected 1 commit");
        assert!(!result.trees.is_empty(), "expected at least 1 tree");
        assert!(!result.blobs.is_empty(), "expected at least 1 blob");
        assert!(result.pointers.is_empty(), "expected no pointers");

        let bounded = walk_reachable_bounded(
            &git_dir_path,
            &[("refs/heads/main".to_owned(), head_sha.clone())],
            1,
        )
        .unwrap_err();
        assert!(matches!(
            bounded,
            WalkError::LimitExceeded {
                actual: 2,
                maximum: 1
            }
        ));

        let tree_bounded = walk_reachable_bounded(
            &git_dir_path,
            &[("refs/heads/main".to_owned(), head_sha.clone())],
            2,
        )
        .unwrap_err();
        assert!(matches!(
            tree_bounded,
            WalkError::LimitExceeded {
                actual: 3,
                maximum: 2
            }
        ));

        let shared = walk_reachable_by_ref_bounded(
            &git_dir_path,
            &[
                ("refs/heads/main".to_owned(), head_sha.clone()),
                ("refs/heads/alias".to_owned(), head_sha.clone()),
            ],
            &BTreeMap::new(),
            result.object_count(),
        )
        .expect("shared ref history should use one distinct-object budget");
        assert_eq!(
            shared["refs/heads/main"].object_count(),
            shared["refs/heads/alias"].object_count()
        );

        let missing_tip = "f".repeat(40);
        let per_ref = walk_reachable_by_ref_bounded(
            &git_dir_path,
            &[
                ("refs/heads/main".to_owned(), head_sha),
                ("refs/heads/concurrent".to_owned(), missing_tip.clone()),
            ],
            &BTreeMap::new(),
            1,
        )
        .unwrap_err();
        assert!(matches!(
            per_ref,
            WalkError::BeyondShallowBoundary { oid } if oid == missing_tip
        ));
    }

    #[cfg(unix)]
    #[test]
    fn walk_reachable_includes_symlink_target_blob() {
        use std::os::unix::fs::symlink;

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
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
        git!("config", "user.email", "test@test.com")
            .status()
            .unwrap();
        git!("config", "user.name", "Test").status().unwrap();
        std::fs::write(repo_dir.join("target.txt"), b"target\n").unwrap();
        symlink("target.txt", repo_dir.join("link.txt")).unwrap();
        git!("add", ".").status().unwrap();
        assert!(git!("commit", "-m", "symlink").status().unwrap().success());

        let head = String::from_utf8(git!("rev-parse", "HEAD").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        let symlink_oid =
            String::from_utf8(git!("rev-parse", "HEAD:link.txt").output().unwrap().stdout)
                .unwrap()
                .trim()
                .to_owned();
        let result =
            walk_reachable(&git_dir_path, &[("refs/heads/main".to_owned(), head)]).unwrap();
        let symlink_oid = ObjectId::from_hex(symlink_oid.as_bytes()).unwrap();
        assert!(result.blobs.contains(&oid_to_bytes(&symlink_oid)));
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
        assert!(
            !result.commits.contains(&gitlink_bytes),
            "submodule gitlinks are not superproject commits"
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
