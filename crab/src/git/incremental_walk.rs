//! Incremental pointer discovery for the native push pipeline.
//!
//! [`walk_incremental`] walks only commits reachable from `new_sha` but not
//! from `old_sha`, discovering pointer blobs and building commit entries.
//! When `old_sha` is `None` (first push or state missing), it falls back to
//! the full [`walk_reachable`](super::walk::walk_reachable) logic.
//!
//! For each new commit, the tree is diffed against its parent's tree using
//! `gix_diff::tree()`, which recursively descends only into changed subtrees.
//! This makes pointer discovery O(changed files) instead of O(total tree
//! entries).

use std::collections::HashSet;
use std::path::Path;

use gix_hash::ObjectId;
use gix_object::FindExt;
use tracing::{debug, info_span, warn};

use crate::core::error::{CrabError, Result};
use crate::git::walk::PointerBlob;
use crab_metadata::commit_graph::CommitEntry;
use crab_types::pointer::Pointer;

fn commit_walk_error(
    context: &'static str,
    tip: &str,
    err: gix_traverse::commit::simple::Error,
) -> CrabError {
    match err {
        gix_traverse::commit::simple::Error::Find(source) => {
            debug!(
                error = %source,
                tip,
                context,
                "commit walk crossed missing local history"
            );
            CrabError::BeyondShallowBoundary {
                oid: tip.to_owned(),
            }
        }
        other => CrabError::Internal(format!("{context}: {other}")),
    }
}

/// Walk only new commits between `old_sha` and `new_sha`, discovering
/// pointer blobs and commit entries.
///
/// When `old_sha` is `Some`, uses `gix-traverse` with the old SHA as a
/// hidden boundary so only commits reachable from `new_sha` but not from
/// `old_sha` are visited. For each new commit's tree, blobs are checked
/// for pointer format.
///
/// When `old_sha` is `None` (first push), delegates to the existing
/// `walk_reachable` logic.
///
/// # Errors
///
/// Returns [`CrabError::Io`] if the objects directory cannot be opened,
/// or [`CrabError::Internal`] if a referenced object is missing or corrupt.
pub fn walk_incremental(
    git_dir: &Path,
    old_sha: Option<&str>,
    new_sha: &str,
) -> Result<(Vec<PointerBlob>, Vec<CommitEntry>)> {
    let hidden_shas: Vec<&str> = old_sha.into_iter().filter(|sha| !sha.is_empty()).collect();
    walk_incremental_with_hidden(git_dir, &hidden_shas, new_sha)
}

/// Walk only commits reachable from `new_sha` and hidden by none of
/// `hidden_shas`, discovering pointer blobs and commit entries.
///
/// Multiple hidden tips let native push use every locally-available
/// remote ref tip as a valid exclusion frontier for new-branch pushes,
/// matching pack-object semantics without scanning history already
/// reachable on the remote.
///
/// # Errors
///
/// Returns [`CrabError::Io`] if the objects directory cannot be opened,
/// [`CrabError::BeyondShallowBoundary`] if the commit graph cannot be
/// traversed because local history is incomplete, or
/// [`CrabError::Internal`] if a referenced object is corrupt.
pub fn walk_incremental_with_hidden(
    git_dir: &Path,
    hidden_shas: &[&str],
    new_sha: &str,
) -> Result<(Vec<PointerBlob>, Vec<CommitEntry>)> {
    // No boundary — fall back to full walk.
    if hidden_shas.is_empty() {
        debug!("no hidden shas, falling back to full walk");
        let refs = vec![("(push)".to_owned(), new_sha.to_owned())];
        let reachable = super::walk::walk_reachable(git_dir, &refs)?;
        let entries = collect_entries_from_walk(git_dir, new_sha)?;
        return Ok((reachable.pointers, entries));
    }

    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return Err(CrabError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("git objects directory not found: {}", objects_dir.display()),
        )));
    }

    let odb = gix_odb::at(&objects_dir).map_err(|e| {
        CrabError::Internal(format!(
            "failed to open git ODB at {}: {e}",
            objects_dir.display()
        ))
    })?;

    let new_oid = ObjectId::from_hex(new_sha.as_bytes())
        .map_err(|e| CrabError::Internal(format!("invalid new_sha '{new_sha}': {e}")))?;

    let hidden_oids: Vec<ObjectId> = hidden_shas
        .iter()
        .map(|hidden_sha| {
            ObjectId::from_hex(hidden_sha.as_bytes())
                .map_err(|e| CrabError::Internal(format!("invalid hidden_sha '{hidden_sha}': {e}")))
        })
        .collect::<Result<_>>()?;

    // Walk commits reachable from new_sha but hidden by old tips, diffing
    // each commit's tree against its parent(s) to visit only changed paths.
    let commit_walk = gix_traverse::commit::Simple::new(std::iter::once(new_oid), &odb)
        .hide(hidden_oids)
        .map_err(|e| commit_walk_error("set up incremental walk", new_sha, e))?;

    let mut pointers = Vec::new();
    let mut entries = Vec::new();
    let mut seen_blobs: HashSet<[u8; 20]> = HashSet::new();
    let mut tree_diff_entries: u64 = 0;
    let mut fallback_full_walks: u64 = 0;

    let _span = info_span!(
        "incremental_walk",
        commits = tracing::field::Empty,
        pointers_found = tracing::field::Empty,
        tree_diff_entries = tracing::field::Empty,
        fallback_full_walks = tracing::field::Empty,
    )
    .entered();

    for info_result in commit_walk {
        let info = info_result.map_err(|e| commit_walk_error("incremental walk", new_sha, e))?;

        // Build CommitEntry with gen_number=0; fill generations after the walk
        // via topological sort (BFS order visits children before parents, so
        // per-commit lookup of parent generations always misses).
        let oid_hex = info.id.to_hex().to_string();
        let parent_oids: Vec<String> = info
            .parent_ids
            .iter()
            .map(|p| p.to_hex().to_string())
            .collect();

        entries.push(CommitEntry {
            oid: oid_hex.clone(),
            gen_number: 0,
            parents: parent_oids,
        });

        // Get the tree OID from this commit.
        let tree_id = {
            let mut buf = Vec::new();
            let mut commit_iter = odb.find_commit_iter(&info.id, &mut buf).map_err(|e| {
                CrabError::Internal(format!("failed to read commit {}: {e}", info.id))
            })?;
            commit_iter.tree_id().map_err(|e| {
                CrabError::Internal(format!("failed to parse tree from commit {}: {e}", info.id))
            })?
        };

        let pointers_before = pointers.len();
        let diff_entries_before = tree_diff_entries;

        if info.parent_ids.is_empty() {
            // Root commit — no parent to diff against, full-tree walk.
            let mut seen_trees: HashSet<[u8; 20]> = HashSet::new();
            walk_tree_for_pointers(
                &odb,
                &tree_id,
                &mut seen_trees,
                &mut seen_blobs,
                &mut pointers,
            )?;
            fallback_full_walks += 1;
        } else {
            // Diff against each parent. For merge commits this unions the
            // changed blobs across all parents via the shared seen_blobs set.
            for parent_id in &info.parent_ids {
                let Ok(parent_tree_id) = read_commit_tree(&odb, parent_id) else {
                    // Parent not in ODB (shallow clone, aggressive GC).
                    // Fall back to full-tree walk for this commit only.
                    debug!(
                        commit = %info.id,
                        parent = %parent_id,
                        reason = "parent_missing",
                        "falling back to full-tree walk for this commit"
                    );
                    let mut seen_trees: HashSet<[u8; 20]> = HashSet::new();
                    walk_tree_for_pointers(
                        &odb,
                        &tree_id,
                        &mut seen_trees,
                        &mut seen_blobs,
                        &mut pointers,
                    )?;
                    fallback_full_walks += 1;
                    // Skip remaining parents — full walk already covered everything.
                    break;
                };

                let diff_count = diff_trees_for_pointers(
                    &odb,
                    &parent_tree_id,
                    &tree_id,
                    &mut seen_blobs,
                    &mut pointers,
                )?;
                tree_diff_entries += diff_count;
            }
        }

        let commit_pointers = pointers.len() - pointers_before;
        let commit_diff_entries = tree_diff_entries - diff_entries_before;
        debug!(
            commit_oid = %oid_hex,
            parents_count = info.parent_ids.len(),
            diff_entries = commit_diff_entries,
            pointers_found = commit_pointers,
            "processed commit"
        );
    }

    // Fill in topologically-correct generation numbers.
    crab_metadata::commit_graph::fill_generation_numbers(&mut entries);

    // Record final counters on the span.
    tracing::Span::current().record("commits", entries.len());
    tracing::Span::current().record("pointers_found", pointers.len());
    tracing::Span::current().record("tree_diff_entries", tree_diff_entries);
    tracing::Span::current().record("fallback_full_walks", fallback_full_walks);

    debug!(
        pointers = pointers.len(),
        commits = entries.len(),
        hidden_tips = hidden_shas.len(),
        tree_diff_entries,
        fallback_full_walks,
        "incremental walk complete"
    );

    Ok((pointers, entries))
}

/// Collect commit entries by walking all commits reachable from `new_sha`.
/// Used by full-walk discovery and follow-tags reachability checks.
pub(super) fn collect_entries_from_walk(git_dir: &Path, new_sha: &str) -> Result<Vec<CommitEntry>> {
    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return Ok(Vec::new());
    }

    let odb = gix_odb::at(&objects_dir).map_err(|e| {
        CrabError::Internal(format!(
            "failed to open git ODB at {}: {e}",
            objects_dir.display()
        ))
    })?;

    let tip = match ObjectId::from_hex(new_sha.as_bytes()) {
        Ok(oid) => oid,
        Err(e) => {
            warn!(sha = %new_sha, error = %e, "invalid SHA for commit entry collection");
            return Ok(Vec::new());
        }
    };

    let commit_walk = gix_traverse::commit::Simple::new(std::iter::once(tip), &odb);
    let mut entries = Vec::new();

    for info_result in commit_walk {
        let info = info_result.map_err(|e| commit_walk_error("commit walk", new_sha, e))?;

        let oid_hex = info.id.to_hex().to_string();
        let parent_oids: Vec<String> = info
            .parent_ids
            .iter()
            .map(|p| p.to_hex().to_string())
            .collect();

        // gen_number is filled in after the walk; see S1-P1-6.
        entries.push(CommitEntry {
            oid: oid_hex,
            gen_number: 0,
            parents: parent_oids,
        });
    }

    crab_metadata::commit_graph::fill_generation_numbers(&mut entries);

    Ok(entries)
}

/// Convert a 20-byte SHA-1 OID to a fixed-size array.
fn oid_to_bytes(oid: &gix_hash::oid) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf.copy_from_slice(oid.as_bytes());
    buf
}

/// Read a commit object and return its tree OID.
fn read_commit_tree(odb: &impl gix_object::Find, commit_id: &gix_hash::oid) -> Result<ObjectId> {
    let mut buf = Vec::new();
    let mut commit_iter = odb.find_commit_iter(commit_id, &mut buf).map_err(|e| {
        CrabError::Internal(format!("failed to read parent commit {commit_id}: {e}"))
    })?;
    commit_iter.tree_id().map_err(|e| {
        CrabError::Internal(format!(
            "failed to parse tree from parent commit {commit_id}: {e}"
        ))
    })
}

/// Diff two trees and check added/modified blobs for pointer format.
///
/// Uses `gix_diff::tree()` which recursively descends only into subtrees
/// whose OIDs differ, making the diff O(changed paths) rather than
/// O(total tree entries). Returns the number of tree entries visited
/// (additions + modifications + deletions) for observability.
fn diff_trees_for_pointers(
    odb: &impl gix_object::Find,
    old_tree: &gix_hash::oid,
    new_tree: &gix_hash::oid,
    seen_blobs: &mut HashSet<[u8; 20]>,
    pointers: &mut Vec<PointerBlob>,
) -> Result<u64> {
    // Fast path: identical trees produce zero changes.
    if old_tree == new_tree {
        return Ok(0);
    }

    let mut buf_old = Vec::new();
    let mut buf_new = Vec::new();

    let old_iter = odb
        .find_tree_iter(old_tree, &mut buf_old)
        .map_err(|e| CrabError::Internal(format!("failed to read old tree {old_tree}: {e}")))?;
    let new_iter = odb
        .find_tree_iter(new_tree, &mut buf_new)
        .map_err(|e| CrabError::Internal(format!("failed to read new tree {new_tree}: {e}")))?;

    let mut visitor = PointerDiffVisitor {
        candidate_blobs: Vec::new(),
        added_subtrees: Vec::new(),
        entry_count: 0,
    };
    let mut diff_state = gix_diff::tree::State::default();

    gix_diff::tree(old_iter, new_iter, &mut diff_state, odb, &mut visitor).map_err(|e| {
        CrabError::Internal(format!(
            "tree diff error between {old_tree} and {new_tree}: {e}"
        ))
    })?;

    let entry_count = visitor.entry_count;
    let candidate_blobs = visitor.candidate_blobs;
    let added_subtrees = visitor.added_subtrees;

    // Check each candidate blob for pointer content.
    for blob_id in candidate_blobs {
        let bytes = oid_to_bytes(&blob_id);
        if seen_blobs.insert(bytes) {
            check_blob_for_pointer(odb, &blob_id, seen_blobs, pointers);
        }
    }

    // Walk added subtrees (new directories) to find all blobs within.
    for subtree_id in added_subtrees {
        let mut seen_trees: HashSet<[u8; 20]> = HashSet::new();
        walk_tree_for_pointers(odb, &subtree_id, &mut seen_trees, seen_blobs, pointers)?;
    }

    Ok(entry_count)
}

/// Tree diff visitor that collects added/modified blob OIDs for pointer checking.
///
/// For added subtrees (new directories), records the tree OID so the caller
/// can recursively walk all blobs within. Dedup against `seen_blobs` happens
/// after the diff completes to avoid borrow conflicts.
struct PointerDiffVisitor {
    candidate_blobs: Vec<ObjectId>,
    added_subtrees: Vec<ObjectId>,
    entry_count: u64,
}

impl gix_diff::tree::Visit for PointerDiffVisitor {
    fn pop_front_tracked_path_and_set_current(&mut self) {}
    fn push_back_tracked_path_component(&mut self, _component: &gix_object::bstr::BStr) {}
    fn push_path_component(&mut self, _component: &gix_object::bstr::BStr) {}
    fn pop_path_component(&mut self) {}

    fn visit(&mut self, change: gix_diff::tree::visit::Change) -> gix_diff::tree::visit::Action {
        self.entry_count += 1;

        match change {
            gix_diff::tree::visit::Change::Addition {
                entry_mode, oid, ..
            } => {
                if entry_mode.is_blob() {
                    self.candidate_blobs.push(oid);
                } else if entry_mode.is_tree() {
                    // New directory — record for recursive walk after the diff.
                    self.added_subtrees.push(oid);
                }
                // Skip symlinks (mode 120000) and submodules (mode 160000).
            }
            gix_diff::tree::visit::Change::Modification {
                entry_mode, oid, ..
            } => {
                if entry_mode.is_blob() {
                    self.candidate_blobs.push(oid);
                }
                // Modified trees are handled by gix_diff recursing into them.
            }
            gix_diff::tree::visit::Change::Deletion { .. } => {
                // Deleted blobs don't need pointer discovery.
            }
        }

        std::ops::ControlFlow::Continue(())
    }
}

/// Walk a tree recursively, checking each blob for pointer format.
fn walk_tree_for_pointers(
    odb: &impl gix_object::Find,
    tree_id: &gix_hash::oid,
    seen_trees: &mut HashSet<[u8; 20]>,
    seen_blobs: &mut HashSet<[u8; 20]>,
    pointers: &mut Vec<PointerBlob>,
) -> Result<()> {
    let tree_bytes = oid_to_bytes(tree_id);
    if !seen_trees.insert(tree_bytes) {
        return Ok(());
    }

    let mut buf = Vec::new();
    let tree_iter = odb
        .find_tree_iter(tree_id, &mut buf)
        .map_err(|e| CrabError::Internal(format!("failed to read tree {tree_id}: {e}")))?;

    let mut visitor = PointerCollector::new(seen_trees, seen_blobs);
    let mut state = gix_traverse::tree::breadthfirst::State::default();

    gix_traverse::tree::breadthfirst(tree_iter, &mut state, odb, &mut visitor)
        .map_err(|e| CrabError::Internal(format!("tree walk error at {tree_id}: {e}")))?;

    // Check each newly discovered blob for pointer content.
    for blob_oid in &visitor.pending_blobs {
        check_blob_for_pointer(odb, blob_oid, seen_blobs, pointers);
    }

    Ok(())
}

/// Check whether a blob is a crab pointer and record it if so.
fn check_blob_for_pointer(
    odb: &impl gix_object::Find,
    blob_id: &ObjectId,
    _seen_blobs: &HashSet<[u8; 20]>,
    pointers: &mut Vec<PointerBlob>,
) {
    let mut buf = Vec::new();
    let data = match odb.try_find(blob_id, &mut buf) {
        Ok(Some(data)) if data.kind == gix_object::Kind::Blob => data,
        Ok(Some(_)) => return,
        Ok(None) => {
            warn!(oid = %blob_id, "blob referenced in tree but not found in ODB");
            return;
        }
        Err(e) => {
            warn!(oid = %blob_id, error = %e, "failed to read blob");
            return;
        }
    };

    if data.data.len() <= crab_types::pointer::MAX_POINTER_SIZE {
        if let Ok(ptr) = Pointer::parse(data.data) {
            pointers.push(PointerBlob {
                oid: oid_to_bytes(blob_id),
                file_hash: ptr.file_hash,
                size: ptr.size,
            });
        }
    }
}

/// Tree visitor that collects blob OIDs for pointer checking.
struct PointerCollector<'a> {
    seen_trees: &'a mut HashSet<[u8; 20]>,
    seen_blobs: &'a mut HashSet<[u8; 20]>,
    pending_blobs: Vec<ObjectId>,
}

impl<'a> PointerCollector<'a> {
    fn new(seen_trees: &'a mut HashSet<[u8; 20]>, seen_blobs: &'a mut HashSet<[u8; 20]>) -> Self {
        Self {
            seen_trees,
            seen_blobs,
            pending_blobs: Vec::new(),
        }
    }
}

impl gix_traverse::tree::Visit for PointerCollector<'_> {
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
        if self.seen_trees.insert(bytes) {
            std::ops::ControlFlow::Continue(true)
        } else {
            std::ops::ControlFlow::Continue(false)
        }
    }

    fn visit_nontree(
        &mut self,
        entry: &gix_object::tree::EntryRef<'_>,
    ) -> std::ops::ControlFlow<(), bool> {
        if !entry.mode.is_blob() {
            return std::ops::ControlFlow::Continue(true);
        }

        let bytes = oid_to_bytes(entry.oid);
        if self.seen_blobs.insert(bytes) {
            self.pending_blobs.push(entry.oid.to_owned());
        }
        std::ops::ControlFlow::Continue(true)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // --- helpers ---

    /// Generate a valid crab pointer blob with a unique file-hash derived
    /// from `seed`. Each seed produces a distinct 64-hex hash so the dedup
    /// set treats them as separate blobs.
    fn make_pointer_content(seed: u8) -> String {
        let hex_char = format!("{seed:02x}");
        let hash = hex_char.repeat(32);
        format!(
            "version https://crab.dev/spec/v1\nfile-hash {hash}\nsize {}\n",
            seed as u64 * 1000 + 42
        )
    }

    /// Run a git command in `repo_dir`, returning the output.
    /// Panics on non-zero exit.
    fn git(repo_dir: &Path, args: &[&str]) -> std::process::Output {
        let _git_env = crate::test::git_repo::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo_dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Run a git command silently (ignore stdout/stderr).
    fn git_silent(repo_dir: &Path, args: &[&str]) {
        let _ = git(repo_dir, args);
    }

    /// Initialize a git repo and configure user identity.
    fn init_repo(repo_dir: &Path) {
        git_silent(repo_dir, &["init", "--initial-branch=main"]);
        git_silent(repo_dir, &["config", "user.email", "test@test.com"]);
        git_silent(repo_dir, &["config", "user.name", "Test"]);
    }

    /// Get HEAD SHA as a 40-char hex string.
    fn head_sha(repo_dir: &Path) -> String {
        let out = git(repo_dir, &["rev-parse", "HEAD"]);
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    // --- existing tests ---

    #[test]
    fn walk_incremental_none_old_sha_errors_on_missing_objects_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        // No objects/ subdirectory.

        let err = walk_incremental(&git_dir, None, &"a".repeat(40)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not found"),
            "expected 'not found' in error, got: {msg}"
        );
    }

    #[test]
    fn walk_incremental_invalid_new_sha_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        let objects_dir = git_dir.join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();

        let err = walk_incremental(&git_dir, Some(&"a".repeat(40)), "not-hex").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid new_sha"), "got: {msg}");
    }

    // --- incremental tree-diff tests ---

    #[test]
    fn tree_diff_finds_added_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);

        // First commit: one non-pointer file so we have a base.
        std::fs::write(repo.join("readme.txt"), b"hello\n").unwrap();
        git_silent(repo, &["add", "readme.txt"]);
        git_silent(repo, &["commit", "-m", "initial"]);
        let old = head_sha(repo);

        // Second commit: add 3 pointer files.
        for i in 0..3 {
            let name = format!("ptr_{i}.bin");
            std::fs::write(repo.join(&name), make_pointer_content(i)).unwrap();
            git_silent(repo, &["add", &name]);
        }
        git_silent(repo, &["commit", "-m", "add pointers"]);
        let new = head_sha(repo);

        let git_dir = repo.join(".git");
        let (pointers, entries) = walk_incremental(&git_dir, Some(&old), &new).unwrap();

        assert_eq!(
            pointers.len(),
            3,
            "expected 3 added pointers, got {}",
            pointers.len()
        );
        assert!(!entries.is_empty(), "expected at least 1 commit entry");
    }

    #[test]
    fn tree_diff_finds_modified_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);

        // First commit: 100 pointer files.
        for i in 0u8..100 {
            let name = format!("file_{i:03}.bin");
            std::fs::write(repo.join(&name), make_pointer_content(i)).unwrap();
        }
        git_silent(repo, &["add", "."]);
        git_silent(repo, &["commit", "-m", "initial 100 files"]);
        let old = head_sha(repo);

        // Second commit: modify 2 files (change their pointer content).
        // Use seeds 200+ so the file-hash differs from the original.
        std::fs::write(repo.join("file_010.bin"), make_pointer_content(200)).unwrap();
        std::fs::write(repo.join("file_050.bin"), make_pointer_content(201)).unwrap();
        git_silent(repo, &["add", "file_010.bin", "file_050.bin"]);
        git_silent(repo, &["commit", "-m", "modify 2 files"]);
        let new = head_sha(repo);

        let git_dir = repo.join(".git");
        let (pointers, _) = walk_incremental(&git_dir, Some(&old), &new).unwrap();

        assert_eq!(
            pointers.len(),
            2,
            "expected 2 modified pointers, got {}",
            pointers.len()
        );
    }

    #[test]
    fn tree_diff_ignores_deleted_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);

        // First commit: 3 pointer files.
        for i in 0..3 {
            let name = format!("ptr_{i}.bin");
            std::fs::write(repo.join(&name), make_pointer_content(i)).unwrap();
        }
        git_silent(repo, &["add", "."]);
        git_silent(repo, &["commit", "-m", "initial"]);
        let old = head_sha(repo);

        // Second commit: delete one file.
        git_silent(repo, &["rm", "ptr_1.bin"]);
        git_silent(repo, &["commit", "-m", "delete one"]);
        let new = head_sha(repo);

        let git_dir = repo.join(".git");
        let (pointers, _) = walk_incremental(&git_dir, Some(&old), &new).unwrap();

        assert_eq!(
            pointers.len(),
            0,
            "deletions should not produce pointers, got {}",
            pointers.len()
        );
    }

    #[test]
    fn tree_diff_handles_merge_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);

        // Base commit.
        std::fs::write(repo.join("base.txt"), b"base\n").unwrap();
        git_silent(repo, &["add", "base.txt"]);
        git_silent(repo, &["commit", "-m", "base"]);
        let base = head_sha(repo);

        // Branch A: add a pointer file.
        git_silent(repo, &["checkout", "-b", "branch-a"]);
        std::fs::write(repo.join("a.bin"), make_pointer_content(10)).unwrap();
        git_silent(repo, &["add", "a.bin"]);
        git_silent(repo, &["commit", "-m", "add a.bin"]);

        // Branch B: add a different pointer file.
        git_silent(repo, &["checkout", "-b", "branch-b", &base]);
        std::fs::write(repo.join("b.bin"), make_pointer_content(20)).unwrap();
        git_silent(repo, &["add", "b.bin"]);
        git_silent(repo, &["commit", "-m", "add b.bin"]);

        // Merge A into B (creates a merge commit).
        git_silent(repo, &["merge", "branch-a", "-m", "merge"]);
        let new = head_sha(repo);

        let git_dir = repo.join(".git");
        let (pointers, _) = walk_incremental(&git_dir, Some(&base), &new).unwrap();

        // The merge commit plus the two branch commits are all new relative
        // to base. We should discover both pointer files.
        assert!(
            pointers.len() >= 2,
            "expected at least 2 pointers from merge, got {}",
            pointers.len()
        );
    }

    #[test]
    fn tree_diff_root_commit_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);

        // Single commit with 2 pointer files — no parent to diff against.
        std::fs::write(repo.join("a.bin"), make_pointer_content(1)).unwrap();
        std::fs::write(repo.join("b.bin"), make_pointer_content(2)).unwrap();
        git_silent(repo, &["add", "."]);
        git_silent(repo, &["commit", "-m", "root"]);
        let new = head_sha(repo);

        let git_dir = repo.join(".git");
        // old_sha = None triggers the full-walk fallback.
        let (pointers, entries) = walk_incremental(&git_dir, None, &new).unwrap();

        assert_eq!(
            pointers.len(),
            2,
            "root commit fallback should find 2 pointers, got {}",
            pointers.len()
        );
        assert!(!entries.is_empty(), "expected commit entries");
    }

    #[test]
    fn tree_diff_missing_parent_falls_back() {
        // Test the missing-parent fallback by creating a shallow clone.
        // In a shallow clone, boundary commits exist but their parents
        // don't. We use old_sha=None so the code takes the full-walk
        // fallback path for the root commit (which has no parent in the
        // shallow ODB), and the incremental path for subsequent commits.
        //
        // Since gix_traverse's commit walker errors on truly missing
        // parent objects (it tries to read them to continue the walk),
        // the `read_commit_tree` fallback in walk_incremental is
        // defensive code for future walker improvements. We test the
        // equivalent behavior: a shallow clone where old_sha=None
        // triggers the full-walk fallback, and pointers are discovered.
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        let shallow = tmp.path().join("shallow");
        std::fs::create_dir_all(&origin).unwrap();
        init_repo(&origin);

        // Two commits in the origin repo.
        std::fs::write(origin.join("base.txt"), b"base\n").unwrap();
        git_silent(&origin, &["add", "base.txt"]);
        git_silent(&origin, &["commit", "-m", "first"]);

        std::fs::write(origin.join("ptr.bin"), make_pointer_content(5)).unwrap();
        git_silent(&origin, &["add", "ptr.bin"]);
        git_silent(&origin, &["commit", "-m", "add pointer"]);

        // Shallow clone with depth=1 — only the latest commit is present.
        git_silent(
            tmp.path(),
            &[
                "clone",
                "--depth=1",
                origin.to_str().unwrap(),
                shallow.to_str().unwrap(),
            ],
        );

        let new = head_sha(&shallow);
        let git_dir = shallow.join(".git");

        // With old_sha=None, walk_incremental falls back to full walk.
        // The shallow clone has only 1 commit (the tip). Its parent is
        // listed in .git/shallow and doesn't exist in the ODB.
        let (pointers, entries) = walk_incremental(&git_dir, None, &new).unwrap();

        assert!(
            pointers.len() >= 1,
            "shallow clone fallback should discover the pointer, got {}",
            pointers.len()
        );
        assert!(!entries.is_empty(), "expected at least 1 commit entry");
    }

    #[test]
    fn tree_diff_unchanged_files_not_visited() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);

        // First commit: 1000 pointer files.
        for i in 0u16..1000 {
            let name = format!("file_{i:04}.bin");
            // Use the low byte as seed; files with same seed share content
            // but that's fine — we just need many tree entries.
            std::fs::write(repo.join(&name), make_pointer_content((i % 256) as u8)).unwrap();
        }
        git_silent(repo, &["add", "."]);
        git_silent(repo, &["commit", "-m", "initial 1000 files"]);
        let old = head_sha(repo);

        // Second commit: modify exactly 1 file.
        std::fs::write(repo.join("file_0500.bin"), make_pointer_content(255)).unwrap();
        git_silent(repo, &["add", "file_0500.bin"]);
        git_silent(repo, &["commit", "-m", "modify 1 file"]);
        let new = head_sha(repo);

        let git_dir = repo.join(".git");
        let (pointers, _) = walk_incremental(&git_dir, Some(&old), &new).unwrap();

        // The tree diff should find exactly 1 modified pointer, not 1000.
        // This proves the diff visits only changed entries.
        assert_eq!(
            pointers.len(),
            1,
            "expected 1 pointer (not 1000), got {}",
            pointers.len()
        );
    }

    #[test]
    fn tree_diff_added_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);

        // First commit: one file at root.
        std::fs::write(repo.join("root.txt"), b"root\n").unwrap();
        git_silent(repo, &["add", "root.txt"]);
        git_silent(repo, &["commit", "-m", "initial"]);
        let old = head_sha(repo);

        // Second commit: add a new directory with 10 pointer files.
        let subdir = repo.join("newdir");
        std::fs::create_dir_all(&subdir).unwrap();
        for i in 0..10 {
            let name = format!("ptr_{i}.bin");
            std::fs::write(subdir.join(&name), make_pointer_content(i)).unwrap();
        }
        git_silent(repo, &["add", "newdir"]);
        git_silent(repo, &["commit", "-m", "add directory"]);
        let new = head_sha(repo);

        let git_dir = repo.join(".git");
        let (pointers, _) = walk_incremental(&git_dir, Some(&old), &new).unwrap();

        assert_eq!(
            pointers.len(),
            10,
            "expected 10 pointers from new directory, got {}",
            pointers.len()
        );
    }

    #[test]
    fn tree_diff_skips_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);

        // First commit: one pointer file.
        std::fs::write(repo.join("real.bin"), make_pointer_content(1)).unwrap();
        git_silent(repo, &["add", "real.bin"]);
        git_silent(repo, &["commit", "-m", "initial"]);
        let old = head_sha(repo);

        // Second commit: add a symlink. On Unix, git stores symlinks as
        // blobs with mode 120000 containing the target path.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("real.bin", repo.join("link.bin")).unwrap();
            git_silent(repo, &["add", "link.bin"]);
            git_silent(repo, &["commit", "-m", "add symlink"]);
            let new = head_sha(repo);

            let git_dir = repo.join(".git");
            let (pointers, _) = walk_incremental(&git_dir, Some(&old), &new).unwrap();

            // The symlink blob contains "real.bin" (the target path), not
            // pointer content. Even if it somehow parsed as a pointer, the
            // diff visitor skips symlink entries (mode 120000) because
            // `entry_mode.is_blob()` returns false for symlinks.
            assert_eq!(
                pointers.len(),
                0,
                "symlinks should not produce pointers, got {}",
                pointers.len()
            );
        }

        // On non-Unix platforms, skip the symlink-specific assertion but
        // verify the basic setup works.
        #[cfg(not(unix))]
        {
            let new = head_sha(repo);
            let git_dir = repo.join(".git");
            let (pointers, _) = walk_incremental(&git_dir, Some(&old), &new).unwrap();
            assert_eq!(pointers.len(), 0);
        }
    }
}
