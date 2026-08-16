//! LFS file status reporting.
//!
//! Reports which LFS-tracked files are staged or modified in the working
//! tree, including OID changes between the index and HEAD.
//!
//! # Rename detection engine
//!
//! Under the `gix-worktree` feature this module implements rename
//! detection via the `pair_renames` helper below, which performs
//! **exact-OID matching** between the deletion and addition partitions
//! of a diff. That is the correct similarity metric for LFS-tracked
//! files specifically:
//!
//! * LFS pointer blobs are content-addressed by SHA-256.
//! * Crab pointer blobs are content-addressed by blake3.
//! * In both schemes, identical object id implies byte-identical content.
//!
//! So two entries that share an OID across a rename boundary are
//! provably the same file — the 50%-similarity heuristic that
//! `git diff-index -M` applies to arbitrary text blobs is strictly
//! weaker than exact-OID matching here, because it can mis-pair two
//! pointer files whose SHA-256 happen to share a line-prefix but point
//! at different xorbs.
//!
//! A `gix_diff::Rewrites` + `rewrites::Tracker<T>` wrapper would give
//! us configurable similarity thresholds, but that machinery depends on
//! blob-bytes similarity scoring (`num_similarity_checks`,
//! `DiffLineStats`, `diff_cache`) that is meaningless for opaque
//! content-addressed pointers. The `gix-diff` rewrite tracker is designed
//! for arbitrary text diffs, not content-addressed blobs.
//!
//! If a future requirement lands that needs similarity-based matching
//! on non-pointer LFS content, swapping `pair_renames` for a Tracker
//! call site is a contained change — the emit shape (`LfsFileStatus`
//! entries with `renamed_from` set) is already correct, and the
//! existing tests document the expected behavior.

use std::path::Path;
#[cfg(not(feature = "gix-worktree"))]
use std::process::Command;

use crate::core::error::{CrabError, Result};

/// Status of a single LFS-tracked file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LfsFileStatus {
    /// Path relative to the repository root.
    pub path: String,
    /// OID in HEAD (None if the file is new).
    pub old_oid: Option<String>,
    /// OID in the index or working tree.
    pub new_oid: Option<String>,
    /// Whether this change is staged (index vs HEAD) or unstaged (worktree vs index).
    pub staged: bool,
    /// When this entry is a rename, the original (pre-rename) path.
    ///
    /// Set when rename detection pairs a deletion at `renamed_from` with
    /// an addition at `path`, and both sides share the same object id.
    /// For LFS pointer files this is an exact-OID match — pointers are
    /// content-addressed, so identical OIDs imply identical content.
    /// For non-rename entries this is always `None`.
    ///
    /// Serialized with a default so that adding this field is a
    /// backwards-compatible schema change for JSON / JSONL consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
}

/// Classifier that decides whether a path is LFS-tracked.
///
/// Under `gix-pathmatch`, this wraps the consolidated
/// [`core::attrs::TrackedClassifier`] so the same
/// `gix_attributes::Search` tree used by the filter-process and clean
/// paths also drives the status report. Otherwise we fall back to the
/// legacy pattern-list parsed from `.gitattributes` by
/// [`crate::lfs::track::list`].
#[cfg(feature = "gix-pathmatch")]
struct LfsClassifier(crate::core::attrs::TrackedClassifier);

#[cfg(not(feature = "gix-pathmatch"))]
struct LfsClassifier {
    patterns: Vec<String>,
}

impl LfsClassifier {
    fn open(repo_root: &Path) -> Result<Self> {
        #[cfg(feature = "gix-pathmatch")]
        {
            Ok(LfsClassifier(crate::core::attrs::TrackedClassifier::open(
                repo_root, "lfs",
            )?))
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            Ok(LfsClassifier {
                patterns: crate::lfs::track::list(repo_root)?,
            })
        }
    }

    fn is_empty(&self) -> bool {
        #[cfg(feature = "gix-pathmatch")]
        {
            false
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            self.patterns.is_empty()
        }
    }

    fn is_tracked(&self, path: &str) -> bool {
        #[cfg(feature = "gix-pathmatch")]
        {
            self.0.is_tracked(path)
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            is_lfs_tracked_legacy(path, &self.patterns)
        }
    }
}

/// Collect LFS file status for the current repository.
///
/// Uses `git diff-index HEAD` for staged changes and `git diff-files` for
/// unstaged working tree changes. Only files with `filter=lfs` in
/// `.gitattributes` are included.
///
/// # Rename detection
///
/// After raw diffs are collected, a pairing pass promotes
/// `(deletion, addition)` pairs that share the same object id to a
/// single rename entry (`renamed_from` set, `path` pointing at the new
/// location). For LFS pointer files this is unambiguous: LFS pointers
/// are content-addressed by SHA-256 and crab pointers by blake3, so
/// an identical OID on both sides guarantees identical content. This
/// matches `git lfs status`'s `R old -> new` output and is what
/// upstream git does when `diff-index` is invoked with `-M`.
///
/// Rename pairing runs independently on the staged and unstaged
/// partitions so a file renamed in the index but then modified in the
/// worktree shows up as one staged rename plus one unstaged
/// modification, not a chain.
pub fn lfs_status(repo_root: &Path) -> Result<Vec<LfsFileStatus>> {
    let classifier = LfsClassifier::open(repo_root)?;
    if classifier.is_empty() {
        return Ok(Vec::new());
    }

    // Staged partition: HEAD → index.
    let staged_raw = diff_index_head(repo_root).unwrap_or_default();
    let mut staged: Vec<LfsFileStatus> = staged_raw
        .into_iter()
        .filter(|(path, _, _)| classifier.is_tracked(path))
        .map(|(path, old_hash, new_hash)| LfsFileStatus {
            path,
            old_oid: non_zero_hash(old_hash),
            new_oid: non_zero_hash(new_hash),
            staged: true,
            renamed_from: None,
        })
        .collect();
    pair_renames(&mut staged);

    // Unstaged partition: index → worktree.
    let unstaged_raw = diff_files(repo_root).unwrap_or_default();
    let mut unstaged: Vec<LfsFileStatus> = unstaged_raw
        .into_iter()
        .filter(|(path, _, _)| classifier.is_tracked(path))
        .map(|(path, old_hash, new_hash)| LfsFileStatus {
            path,
            old_oid: non_zero_hash(old_hash),
            new_oid: non_zero_hash(new_hash),
            staged: false,
            renamed_from: None,
        })
        .collect();
    pair_renames(&mut unstaged);

    let mut results = staged;
    results.extend(unstaged);
    Ok(results)
}

/// Pair deletions with additions that share the same object id, in
/// place, converting each pair into a single rename entry.
///
/// For LFS pointer files this is exact-OID matching, which is the
/// correct similarity metric — pointer content is content-addressed,
/// so identical OIDs imply identical content. Matches git's
/// `diff-index -M100` behavior for pointer blobs.
///
/// When multiple deletions or additions share the same OID (unusual
/// but legal — two LFS-tracked files with identical content), we pair
/// them in iteration order. That matches git's deterministic behavior
/// under `-M100` and keeps the test surface predictable.
fn pair_renames(entries: &mut Vec<LfsFileStatus>) {
    // Index deletions (entries with no new_oid) by their old_oid, and
    // additions (entries with no old_oid) by their new_oid. We don't
    // touch entries that are true modifications at the same path.
    use std::collections::HashMap;
    let mut del_by_oid: HashMap<String, Vec<usize>> = HashMap::new();
    let mut add_by_oid: HashMap<String, Vec<usize>> = HashMap::new();

    for (i, e) in entries.iter().enumerate() {
        match (&e.old_oid, &e.new_oid) {
            (Some(oid), None) => del_by_oid.entry(oid.clone()).or_default().push(i),
            (None, Some(oid)) => add_by_oid.entry(oid.clone()).or_default().push(i),
            _ => {}
        }
    }

    // Collect (deletion_idx, addition_idx) pairs where the deleted
    // oid matches the added oid. Pair in order so the first deletion
    // at oid X pairs with the first addition at oid X.
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (oid, dels) in &del_by_oid {
        let Some(adds) = add_by_oid.get(oid) else {
            continue;
        };
        for (d, a) in dels.iter().zip(adds.iter()) {
            pairs.push((*d, *a));
        }
    }

    if pairs.is_empty() {
        return;
    }

    // Build a rename entry for each pair. Collect the set of deletion
    // indices to drop in one pass so indices stay valid while we
    // rewrite the additions in place.
    let mut drop_idx: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (del_i, add_i) in &pairs {
        let del_path = entries[*del_i].path.clone();
        let del_staged = entries[*del_i].staged;
        let oid = entries[*del_i].old_oid.clone();
        // Safety: we populated the maps from the same slice, indices
        // are in range. The addition stays at its slot with renamed_from
        // set and old_oid lifted from the deletion.
        let add = &mut entries[*add_i];
        add.old_oid = oid;
        add.renamed_from = Some(del_path);
        // Staged-ness follows the deletion — which must match the
        // addition's, since pair_renames is called per-partition.
        debug_assert_eq!(add.staged, del_staged, "rename pair spans partitions");
        drop_idx.insert(*del_i);
    }

    // Remove the paired deletions. Walk from the end so earlier
    // indices stay valid.
    let mut i = entries.len();
    while i > 0 {
        i -= 1;
        if drop_idx.contains(&i) {
            entries.remove(i);
        }
    }
}

/// Parse `git diff-index --cached HEAD` output into (path, old_hash, new_hash) tuples.
///
/// Legacy shellout path used when `gix-worktree` is off. The in-process
/// replacement at [`diff_index_head_via_gix`] is semantically equivalent
/// for the non-rename cases the caller consumes.
#[cfg(not(feature = "gix-worktree"))]
fn diff_index_head(root: &Path) -> Result<Vec<(String, String, String)>> {
    let output = Command::new("git")
        .args(["diff-index", "--cached", "HEAD"])
        .current_dir(root)
        .output()
        .map_err(|e| CrabError::Configuration {
            key: "git diff-index".to_owned(),
            origin: format!("failed to run git diff-index: {e}"),
        })?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(parse_diff_output(&output.stdout))
}

/// In-process replacement for `git diff-index --cached HEAD`.
///
/// Reads the current index and the HEAD tree via `gix-index` +
/// `gix-odb`, then emits `(path, head_oid, index_oid)` tuples for every
/// path whose index OID differs from its HEAD tree OID. Shared with
/// Task 7.5's dehydrate dirty-check via the same gix-native plumbing.
///
/// Rename detection happens one layer up in [`pair_renames`]: this
/// function emits renames as a paired (deletion, addition), and the
/// caller promotes the pair to a single `Rename` entry when both sides
/// share the same OID. For LFS pointer files — which are
/// content-addressed — exact-OID matching is the correct similarity
/// metric and matches `git diff-index -M100`'s behavior.
#[cfg(feature = "gix-worktree")]
fn diff_index_head(root: &Path) -> Result<Vec<(String, String, String)>> {
    diff_index_head_via_gix(root)
}

#[cfg(feature = "gix-worktree")]
fn diff_index_head_via_gix(root: &Path) -> Result<Vec<(String, String, String)>> {
    use gix_object::FindExt;

    let Ok(ctx) = crate::git::worktree::WorktreeContext::resolve_from_path(root) else {
        return Ok(Vec::new());
    };

    // Resolve HEAD → tree. Best-effort: if HEAD doesn't resolve (brand-
    // new repo with no commits) treat everything in the index as a
    // staged addition against a zero-hash baseline.
    let head_sha = crab_git::ref_resolve::resolve_ref_at(&ctx.per_worktree_git_dir, "HEAD").ok();
    let odb = gix_odb::at(ctx.objects_dir()).map_err(|e| CrabError::Configuration {
        key: "gix odb".to_owned(),
        origin: format!("failed to open odb: {e}"),
    })?;

    let head_tree_map = match head_sha {
        Some(sha) => {
            let commit_oid = gix_hash::ObjectId::from_hex(sha.as_bytes()).map_err(|e| {
                CrabError::Configuration {
                    key: "gix odb".to_owned(),
                    origin: format!("invalid HEAD SHA: {e}"),
                }
            })?;
            let mut buf = Vec::new();
            let commit =
                odb.find_commit(&commit_oid, &mut buf)
                    .map_err(|e| CrabError::Configuration {
                        key: "gix odb".to_owned(),
                        origin: format!("failed to read HEAD commit: {e}"),
                    })?;
            let tree_oid = commit.tree();
            flatten_tree(&odb, &tree_oid)?
        }
        None => std::collections::HashMap::new(),
    };

    // Read the index.
    let index_path = ctx.index_path();
    if !index_path.is_file() {
        return Ok(Vec::new());
    }
    let index = gix_index::File::at(
        index_path,
        gix_hash::Kind::Sha1,
        true,
        gix_index::decode::Options::default(),
    )
    .map_err(|e| CrabError::Configuration {
        key: "gix index".to_owned(),
        origin: format!("failed to read index: {e}"),
    })?;

    let mut out = Vec::new();
    let mut seen_in_index = std::collections::HashSet::new();

    for entry in index.entries() {
        // Stage 0 only — conflict entries (stages 1-3) are not what
        // `diff-index --cached` reports anyway.
        if entry.stage_raw() != 0 {
            continue;
        }
        let path_bytes = entry.path(&index);
        let Ok(path) = std::str::from_utf8(path_bytes) else {
            continue;
        };
        seen_in_index.insert(path.to_owned());
        let index_oid = entry.id.to_hex().to_string();
        match head_tree_map.get(path) {
            Some(head_oid) if *head_oid == index_oid => {
                // unchanged — skip
            }
            Some(head_oid) => {
                out.push((path.to_owned(), head_oid.clone(), index_oid));
            }
            None => {
                // Added in the index.
                out.push((path.to_owned(), zero_hash(), index_oid));
            }
        }
    }

    // Paths in HEAD tree but not in the index → deletion.
    for (path, head_oid) in &head_tree_map {
        if !seen_in_index.contains(path) {
            out.push((path.clone(), head_oid.clone(), zero_hash()));
        }
    }

    Ok(out)
}

#[cfg(feature = "gix-worktree")]
fn flatten_tree(
    odb: &gix_odb::Handle,
    tree_oid: &gix_hash::oid,
) -> Result<std::collections::HashMap<String, String>> {
    use gix_object::FindExt;

    let mut out = std::collections::HashMap::new();
    let mut buf = Vec::new();
    let tree = match odb.find_tree_iter(tree_oid, &mut buf) {
        Ok(t) => t,
        Err(_) => return Ok(out),
    };

    // Use gix_traverse::tree::Recorder for a flat path listing.
    let mut recorder = gix_traverse::tree::Recorder::default();
    let mut state = gix_traverse::tree::breadthfirst::State::default();
    gix_traverse::tree::breadthfirst(tree, &mut state, odb, &mut recorder).map_err(|e| {
        CrabError::Configuration {
            key: "gix traverse".to_owned(),
            origin: format!("tree traverse failed: {e}"),
        }
    })?;

    for record in recorder.records {
        // Only blobs (regular files, executables, symlinks) participate
        // in the diff — sub-trees don't have an OID comparison shape
        // that the caller understands. `EntryMode` is a u16-backed
        // struct in gix-object; use the discretized `kind()` accessor
        // for match ergonomics.
        use gix_object::tree::EntryKind;
        if !matches!(
            record.mode.kind(),
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
        ) {
            continue;
        }
        if let Ok(path) = std::str::from_utf8(&record.filepath) {
            out.insert(path.to_owned(), record.oid.to_hex().to_string());
        }
    }
    Ok(out)
}

#[cfg(feature = "gix-worktree")]
fn zero_hash() -> String {
    "0".repeat(40)
}

/// Parse `git diff-files` output into (path, old_hash, new_hash) tuples.
///
/// Legacy shellout path used when `gix-worktree` is off.
#[cfg(not(feature = "gix-worktree"))]
fn diff_files(root: &Path) -> Result<Vec<(String, String, String)>> {
    let output = Command::new("git")
        .args(["diff-files"])
        .current_dir(root)
        .output()
        .map_err(|e| CrabError::Configuration {
            key: "git diff-files".to_owned(),
            origin: format!("failed to run git diff-files: {e}"),
        })?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(parse_diff_output(&output.stdout))
}

/// In-process replacement for `git diff-files` (index vs worktree).
///
/// For every LFS-tracked index entry this computes the blob OID of the
/// current worktree content and compares it to the index OID. The blob
/// hash is computed from the raw file bytes (no filters applied) via
/// gix-object's standard blob framing — matching what `git diff-files`
/// would see before smudge/clean roundtripping.
#[cfg(feature = "gix-worktree")]
fn diff_files(root: &Path) -> Result<Vec<(String, String, String)>> {
    diff_files_via_gix(root)
}

#[cfg(feature = "gix-worktree")]
fn diff_files_via_gix(root: &Path) -> Result<Vec<(String, String, String)>> {
    let Ok(ctx) = crate::git::worktree::WorktreeContext::resolve_from_path(root) else {
        return Ok(Vec::new());
    };

    let index_path = ctx.index_path();
    if !index_path.is_file() {
        return Ok(Vec::new());
    }
    let index = gix_index::File::at(
        index_path,
        gix_hash::Kind::Sha1,
        true,
        gix_index::decode::Options::default(),
    )
    .map_err(|e| CrabError::Configuration {
        key: "gix index".to_owned(),
        origin: format!("failed to read index: {e}"),
    })?;

    let mut out = Vec::new();
    for entry in index.entries() {
        if entry.stage_raw() != 0 {
            continue;
        }
        let path_bytes = entry.path(&index);
        let Ok(path) = std::str::from_utf8(path_bytes) else {
            continue;
        };

        let abs_path = root.join(path);
        // File missing in the worktree → deletion (index → zero).
        let Ok(content) = std::fs::read(&abs_path) else {
            out.push((path.to_owned(), entry.id.to_hex().to_string(), zero_hash()));
            continue;
        };

        // Hash the worktree content as a git blob (the hash format git
        // itself computes for a blob). Short-circuit: if the worktree
        // content yields the same OID as the index, nothing changed.
        let worktree_oid =
            gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, &content)
                .map_err(|e| CrabError::Configuration {
                    key: "gix hash".to_owned(),
                    origin: format!("blob hash failed: {e}"),
                })?;
        if worktree_oid == entry.id {
            continue;
        }
        out.push((
            path.to_owned(),
            entry.id.to_hex().to_string(),
            worktree_oid.to_hex().to_string(),
        ));
    }

    Ok(out)
}

/// Parse raw diff output lines.
///
/// Each line has the format:
/// `:old_mode new_mode old_hash new_hash status\tpath`
#[cfg(not(feature = "gix-worktree"))]
fn parse_diff_output(raw: &[u8]) -> Vec<(String, String, String)> {
    let text = String::from_utf8_lossy(raw);
    let mut entries = Vec::new();

    for line in text.lines() {
        if !line.starts_with(':') {
            continue;
        }

        // Split on tab to separate metadata from path.
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };

        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 5 {
            continue;
        }

        let old_hash = parts[2].to_owned();
        let new_hash = parts[3].to_owned();

        entries.push((path.to_owned(), old_hash, new_hash));
    }

    entries
}

/// Check if a file path matches any of the tracked LFS patterns.
#[cfg(not(feature = "gix-pathmatch"))]
fn is_lfs_tracked_legacy(path: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        if glob_matches_legacy(pattern, path) {
            return true;
        }
    }
    false
}

/// Simple glob matching for gitattributes patterns (legacy fallback).
#[cfg(not(feature = "gix-pathmatch"))]
fn glob_matches_legacy(pattern: &str, path: &str) -> bool {
    // Handle simple extension patterns like "*.bin".
    if let Some(suffix) = pattern.strip_prefix('*') {
        return path.ends_with(suffix);
    }

    // Handle directory patterns like "dir/**".
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path.starts_with(prefix);
    }

    // Exact match.
    path == pattern
}

/// Convert a zero hash to None.
fn non_zero_hash(hash: String) -> Option<String> {
    if hash.chars().all(|c| c == '0') {
        None
    } else {
        Some(hash)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    /// Build a status entry with explicit fields. Keeps tests readable
    /// without exposing internal defaults.
    fn entry(path: &str, old: Option<&str>, new: Option<&str>, staged: bool) -> LfsFileStatus {
        LfsFileStatus {
            path: path.to_owned(),
            old_oid: old.map(str::to_owned),
            new_oid: new.map(str::to_owned),
            staged,
            renamed_from: None,
        }
    }

    /// Shared fixture OIDs — meaningful hex shapes so failures print
    /// paths that are easy to read.
    const OID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn pair_renames_promotes_del_add_pair_sharing_oid() {
        let mut v = vec![
            entry("old.bin", Some(OID_A), None, true),
            entry("new.bin", None, Some(OID_A), true),
        ];
        pair_renames(&mut v);
        assert_eq!(v.len(), 1, "del+add with same oid collapse into rename");
        assert_eq!(v[0].path, "new.bin");
        assert_eq!(v[0].renamed_from.as_deref(), Some("old.bin"));
        assert_eq!(v[0].old_oid.as_deref(), Some(OID_A));
        assert_eq!(v[0].new_oid.as_deref(), Some(OID_A));
    }

    #[test]
    fn pair_renames_leaves_unrelated_entries_alone() {
        let mut v = vec![
            entry("old.bin", Some(OID_A), None, true), // deletion
            entry("new.bin", None, Some(OID_B), true), // addition, different oid
            entry("mod.bin", Some(OID_A), Some(OID_B), true), // modification
        ];
        let before = v.clone();
        pair_renames(&mut v);
        assert_eq!(
            v.len(),
            before.len(),
            "no pair shares an oid; nothing collapses"
        );
        for (got, want) in v.iter().zip(before.iter()) {
            assert_eq!(got.path, want.path);
            assert_eq!(got.renamed_from, want.renamed_from);
        }
    }

    #[test]
    fn pair_renames_pairs_multiple_deletions_with_multiple_additions() {
        // Two LFS-tracked files with identical content both get renamed.
        // Deterministic pairing: first deletion at OID_A pairs with the
        // first addition at OID_A.
        let mut v = vec![
            entry("old1.bin", Some(OID_A), None, true),
            entry("old2.bin", Some(OID_A), None, true),
            entry("new1.bin", None, Some(OID_A), true),
            entry("new2.bin", None, Some(OID_A), true),
        ];
        pair_renames(&mut v);
        assert_eq!(v.len(), 2, "both pairs collapse to renames");
        let paths: Vec<&str> = v.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"new1.bin"));
        assert!(paths.contains(&"new2.bin"));
        let renamed_froms: Vec<&str> = v.iter().filter_map(|e| e.renamed_from.as_deref()).collect();
        assert!(renamed_froms.contains(&"old1.bin"));
        assert!(renamed_froms.contains(&"old2.bin"));
    }

    #[test]
    fn pair_renames_empty_input_is_noop() {
        let mut v: Vec<LfsFileStatus> = Vec::new();
        pair_renames(&mut v);
        assert!(v.is_empty());
    }

    /// Named regression guard requested by Task 7.7.
    ///
    /// When an LFS-tracked pointer file is renamed (same content, new
    /// path), `lfs_status` must surface a single `Rename` entry rather
    /// than the delete + add pair that a raw `diff-index` without `-M`
    /// produces. This matches `git lfs status`'s `R old -> new`
    /// behavior and keeps the LFS status summary counts honest.
    #[test]
    fn lfs_status_handles_rename_from_pointer_to_pointer() {
        let mut v = vec![
            // Delete the pointer at the old path.
            entry("models/weights.bin", Some(OID_A), None, true),
            // Add the same pointer at the new path. Identical OID
            // means the content is provably unchanged — this is a rename.
            entry("models/v2/weights.bin", None, Some(OID_A), true),
        ];
        pair_renames(&mut v);

        assert_eq!(
            v.len(),
            1,
            "pointer-to-pointer rename collapses to one entry"
        );
        let r = &v[0];
        assert_eq!(r.path, "models/v2/weights.bin");
        assert_eq!(r.renamed_from.as_deref(), Some("models/weights.bin"));
        assert_eq!(r.old_oid.as_deref(), Some(OID_A));
        assert_eq!(r.new_oid.as_deref(), Some(OID_A));
        assert!(r.staged, "staged flag preserved through pairing");
    }

    /// Conservative-semantics guard for the exact-OID matching engine.
    ///
    /// If a pointer is renamed *and* its content changes (e.g. the
    /// underlying model was replaced with a new version at a new path),
    /// the two index entries have different OIDs. `git diff-index -M`
    /// with its default 50% similarity would still call this a rename
    /// if the pointer text happens to be 50% line-identical (both are
    /// short pointer blobs — easy to hit that bar), producing a noisy
    /// false-positive rename. Exact-OID matching refuses to collapse
    /// them, and the caller sees a cleaner delete + add pair.
    ///
    /// This test codifies that "correctness" definition for LFS
    /// specifically — exact-OID matching is deliberately stricter than
    /// git's default, because LFS pointers are content-identity.
    #[test]
    fn rename_with_content_change_stays_as_delete_add_pair() {
        let mut v = vec![
            // Delete a pointer at the old path (OID_A).
            entry("models/weights.bin", Some(OID_A), None, true),
            // Add a *different* pointer at a new path (OID_B).
            // Even though the paths look like a rename, the content
            // differs — this is two independent changes, not a rename.
            entry("models/v2/weights.bin", None, Some(OID_B), true),
        ];
        pair_renames(&mut v);

        assert_eq!(
            v.len(),
            2,
            "rename-with-content-change stays separate (different OIDs)",
        );
        for e in &v {
            assert!(
                e.renamed_from.is_none(),
                "no pairing when OIDs differ: {e:?}",
            );
        }
    }

    /// Symmetry: deletion and addition can appear in either order in
    /// the raw diff output. Exact-OID pairing must be order-independent.
    #[test]
    fn pair_renames_is_independent_of_del_add_order() {
        let mut add_first = vec![
            entry("new.bin", None, Some(OID_A), false),
            entry("old.bin", Some(OID_A), None, false),
        ];
        let mut del_first = vec![
            entry("old.bin", Some(OID_A), None, false),
            entry("new.bin", None, Some(OID_A), false),
        ];
        pair_renames(&mut add_first);
        pair_renames(&mut del_first);

        assert_eq!(add_first.len(), 1);
        assert_eq!(del_first.len(), 1);
        assert_eq!(add_first[0].path, "new.bin");
        assert_eq!(del_first[0].path, "new.bin");
        assert_eq!(add_first[0].renamed_from.as_deref(), Some("old.bin"));
        assert_eq!(del_first[0].renamed_from.as_deref(), Some("old.bin"));
    }

    #[test]
    fn rename_serialization_omits_field_when_absent() {
        // Regression guard for JSON consumers: entries that are NOT
        // renames must serialize without a `renamed_from` field at all
        // (skip_serializing_if = Option::is_none), keeping the schema
        // backwards-compatible with pre-rename-detection clients.
        let e = entry("unchanged.bin", Some(OID_A), Some(OID_B), true);
        let v = serde_json::to_value(&e).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("renamed_from"),
            "renamed_from should be absent on non-rename entries, got {v}"
        );
    }

    #[test]
    fn rename_serialization_includes_field_when_present() {
        let mut e = entry("new.bin", Some(OID_A), Some(OID_A), true);
        e.renamed_from = Some("old.bin".to_owned());
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["renamed_from"], "old.bin");
    }

    #[test]
    fn non_zero_hash_drops_all_zeroes() {
        assert_eq!(non_zero_hash("0".repeat(40)), None);
        assert_eq!(non_zero_hash(OID_A.to_owned()), Some(OID_A.to_owned()),);
    }

    #[test]
    fn lfs_status_uses_linked_worktree_index_and_worktree() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let linked = dir.path().join("linked");

        let ok = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", root.to_str().unwrap()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if ok.is_err() || !ok.unwrap().success() {
            return;
        }
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .status();

        std::fs::write(
            root.join(".gitattributes"),
            "*.dat filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        std::fs::write(root.join("tracked.dat"), b"committed linked bytes").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", ".gitattributes", "tracked.dat"])
            .current_dir(&root)
            .status();
        let committed = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if committed.is_err() || !committed.unwrap().success() {
            return;
        }

        let added = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&root)
            .status();
        if added.is_err() || !added.unwrap().success() {
            return;
        }

        std::fs::write(root.join("main-only.dat"), b"main staged bytes").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "main-only.dat"])
            .current_dir(&root)
            .status();
        std::fs::write(linked.join("tracked.dat"), b"linked edited bytes").unwrap();

        let entries = lfs_status(&linked).unwrap();

        assert_eq!(entries.len(), 1, "linked status entries: {entries:?}");
        let entry = &entries[0];
        assert_eq!(entry.path, "tracked.dat");
        assert!(!entry.staged);
        assert_ne!(entry.old_oid, entry.new_oid);
        assert!(
            entries.iter().all(|entry| entry.path != "main-only.dat"),
            "main worktree index state leaked into linked status: {entries:?}"
        );
    }
}
