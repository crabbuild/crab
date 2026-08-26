//! `crab lfs fsck` — verify integrity of local LFS objects and pointers.
//!
//! Three modes of operation:
//! - Default (no flags): verify pointers in HEAD and all local LFS objects.
//! - `--pointers`: verify LFS pointers in the selected revision are canonical.
//! - `--objects`: verify selected LFS objects and skip pointer checks.
//!
//! Revision-scoped object checks verify the objects referenced by that revision
//! or `A..B` range. No-revision object checks preserve Crab's existing local
//! store scan over `.git/lfs/objects/`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::core::error::{CrabError, Result};
use crab_git::lfs_pointer::{LfsPointer, hex_encode};

/// Options for `crab lfs fsck`.
#[derive(Debug, Clone, Default)]
pub struct LfsFsckOptions {
    /// Revision or A..B range to inspect.
    pub revision: Option<String>,
    /// Check pointer canonicality.
    pub pointers: bool,
    /// Check object existence and integrity.
    pub objects: bool,
    /// Report corrupt objects without moving them to `.git/lfs/bad`.
    pub dry_run: bool,
}

/// Run `crab lfs fsck`.
///
/// When both `pointers` and `objects` are false (the default invocation),
/// verifies both pointers and objects. When only one flag is set, runs that
/// specific check.
pub fn run_lfs_fsck(options: LfsFsckOptions) -> Result<()> {
    if options.dry_run {
        tracing::debug!("fsck dry-run requested; corrupt objects will not be moved");
    }

    let check_pointers = options.pointers || !options.objects;
    let check_objects = options.objects || !options.pointers;
    let selected_entries = if check_pointers || options.revision.is_some() {
        Some(selected_tree_entries(options.revision.as_deref())?)
    } else {
        None
    };

    let mut errors_found = 0u64;

    if check_pointers {
        let tree_entries = selected_entries.as_deref().unwrap_or_default();
        errors_found += fsck_pointers(tree_entries)?;
    }

    if check_objects {
        errors_found += if let Some(tree_entries) = selected_entries.as_deref()
            && options.revision.is_some()
        {
            fsck_objects_for_pointers(tree_entries, options.dry_run)?
        } else {
            fsck_objects(options.dry_run)?
        };
    }

    if errors_found > 0 {
        eprintln!("crab lfs fsck: {errors_found} error(s) found");
        return Err(CrabError::LfsObjectCorrupt {
            oid: format!("{errors_found} object(s) failed verification"),
        });
    }

    println!("crab lfs fsck: all OK");
    Ok(())
}

/// Verify all LFS pointers in the selected tree are well-formed and canonical.
///
/// Uses git object listings to enumerate blobs, then `git cat-file -p`
/// to read each blob and attempt to parse it as an LFS pointer.
fn fsck_pointers(tree_entries: &[(String, String)]) -> Result<u64> {
    let mut error_count = 0u64;
    let mut checked = 0u64;

    for (blob_hash, filename) in tree_entries {
        let Some((pointer, blob)) = read_lfs_pointer(blob_hash)? else {
            continue;
        };
        checked += 1;

        // Check that the pointer is well-formed (parse succeeded) and canonical.
        if !LfsPointer::is_canonical(&blob) {
            let oid_hex = hex_encode(&pointer.oid);
            eprintln!(
                "  pointer not canonical: {filename} (oid {oid_short})",
                oid_short = &oid_hex[..10],
            );
            error_count += 1;
        }
    }

    if error_count == 0 {
        println!("  pointers: all {checked} pointer(s) OK");
    }

    Ok(error_count)
}

/// Verify SHA-256 of all locally-stored LFS objects matches their OID.
///
/// Scans `.git/lfs/objects/` for the standard fan-out layout
/// `{aa}/{bb}/{oid}` and re-hashes each file.
fn fsck_objects(dry_run: bool) -> Result<u64> {
    let lfs_objects_dir = discover_lfs_objects_dir()?;

    if !lfs_objects_dir.is_dir() {
        println!("  objects: no local LFS objects directory found");
        return Ok(0);
    }

    let mut checked = 0u64;
    let mut error_count = 0u64;

    // Walk the two-level fan-out: {aa}/{bb}/{oid}
    let top_entries = read_dir_sorted(&lfs_objects_dir)?;
    for aa_entry in &top_entries {
        let aa_path = lfs_objects_dir.join(aa_entry);
        if !aa_path.is_dir() {
            continue;
        }

        let bb_entries = read_dir_sorted(&aa_path)?;
        for bb_entry in &bb_entries {
            let bb_path = aa_path.join(bb_entry);
            if !bb_path.is_dir() {
                continue;
            }

            let obj_entries = read_dir_sorted(&bb_path)?;
            for obj_name in &obj_entries {
                let obj_path = bb_path.join(obj_name);
                if !obj_path.is_file() {
                    continue;
                }

                checked += 1;
                if !verify_object_file(&obj_path, obj_name)? {
                    eprintln!("  corrupt: {obj_name}");
                    move_corrupt_object(&obj_path, obj_name, &lfs_objects_dir, dry_run)?;
                    error_count += 1;
                }
            }
        }
    }

    if error_count == 0 {
        println!("  objects: {checked} object(s) verified OK");
    }

    Ok(error_count)
}

/// Verify selected pointer objects exist locally and match their OID.
fn fsck_objects_for_pointers(tree_entries: &[(String, String)], dry_run: bool) -> Result<u64> {
    let lfs_objects_dir = discover_lfs_objects_dir()?;
    let mut seen = HashSet::new();
    let mut checked = 0u64;
    let mut error_count = 0u64;

    for (blob_hash, filename) in tree_entries {
        let Some((pointer, _blob)) = read_lfs_pointer(blob_hash)? else {
            continue;
        };

        if !seen.insert(pointer.oid) {
            continue;
        }

        checked += 1;
        let oid_hex = hex_encode(&pointer.oid);
        let obj_path = lfs_object_path(&lfs_objects_dir, &oid_hex);
        if !obj_path.is_file() {
            eprintln!(
                "  missing: {filename} (oid {oid_short})",
                oid_short = &oid_hex[..10],
            );
            error_count += 1;
            continue;
        }

        if !verify_object_file(&obj_path, &oid_hex)? {
            eprintln!(
                "  corrupt: {filename} (oid {oid_short})",
                oid_short = &oid_hex[..10],
            );
            move_corrupt_object(&obj_path, &oid_hex, &lfs_objects_dir, dry_run)?;
            error_count += 1;
        }
    }

    if error_count == 0 {
        println!("  objects: {checked} referenced object(s) verified OK");
    }

    Ok(error_count)
}

/// Verify a single LFS object file: re-hash and compare to the filename OID.
fn verify_object_file(path: &Path, expected_oid_hex: &str) -> Result<bool> {
    let content = std::fs::read(path)
        .map_err(|e| CrabError::Internal(format!("failed to read {}: {e}", path.display())))?;

    let mut hasher = Sha256::new();
    hasher.update(&content);
    let computed = hasher.finalize();
    let computed_hex = format!("{computed:064x}");

    if computed_hex != expected_oid_hex {
        return Ok(false);
    }

    Ok(true)
}

fn move_corrupt_object(
    path: &Path,
    oid_hex: &str,
    lfs_objects_dir: &Path,
    dry_run: bool,
) -> Result<()> {
    let bad_path = bad_object_path(lfs_objects_dir, oid_hex);
    if dry_run {
        eprintln!("    would move to {}", bad_path.display());
        return Ok(());
    }

    let Some(bad_dir) = bad_path.parent() else {
        return Err(CrabError::Internal(
            "failed to resolve .git/lfs/bad directory".to_owned(),
        ));
    };
    std::fs::create_dir_all(bad_dir).map_err(|e| {
        CrabError::Internal(format!(
            "failed to create bad LFS object directory {}: {e}",
            bad_dir.display()
        ))
    })?;
    let target = unique_bad_object_path(&bad_path);
    std::fs::rename(path, &target).map_err(|e| {
        CrabError::Internal(format!(
            "failed to move corrupt LFS object {} to {}: {e}",
            path.display(),
            target.display()
        ))
    })?;
    eprintln!("    moved to {}", target.display());
    Ok(())
}

fn bad_object_path(lfs_objects_dir: &Path, oid_hex: &str) -> PathBuf {
    let Some(lfs_dir) = lfs_objects_dir.parent() else {
        return lfs_objects_dir.join("bad").join(oid_hex);
    };
    lfs_dir.join("bad").join(oid_hex)
}

fn unique_bad_object_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }

    for n in 1.. {
        let candidate = path.with_extension(n.to_string());
        if !candidate.exists() {
            return candidate;
        }
    }

    path.to_path_buf()
}

/// Return `(blob_hash, filename)` pairs for HEAD, one revision, or one A..B range.
fn selected_tree_entries(revision: Option<&str>) -> Result<Vec<(String, String)>> {
    match revision {
        Some(revision) => ls_tree_revision(revision),
        None => ls_tree_ref("HEAD"),
    }
}

fn ls_tree_revision(revision: &str) -> Result<Vec<(String, String)>> {
    if revision.contains("...") {
        return Err(CrabError::Configuration {
            key: "only A..B ranges are supported".to_owned(),
            origin: "crab lfs fsck".to_owned(),
        });
    }

    if revision.contains("..") {
        return ls_tree_range(revision);
    }

    ls_tree_ref(revision)
}

/// Run `git ls-tree -r <revision>` and return `(blob_hash, filename)` pairs.
fn ls_tree_ref(revision: &str) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", revision])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git ls-tree: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Not a valid object name") {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!("git ls-tree failed: {stderr}")));
    }

    Ok(parse_ls_tree_output(&output.stdout))
}

/// Run `git rev-list --objects <A..B>` and return object/path pairs.
fn ls_tree_range(range: &str) -> Result<Vec<(String, String)>> {
    let Some((exclude, include)) = range.split_once("..") else {
        return Err(CrabError::Configuration {
            key: "expected A..B revision range".to_owned(),
            origin: "crab lfs fsck".to_owned(),
        });
    };

    if exclude.is_empty() || include.is_empty() || include.contains("..") {
        return Err(CrabError::Configuration {
            key: "expected A..B revision range".to_owned(),
            origin: "crab lfs fsck".to_owned(),
        });
    }

    let output = Command::new("git")
        .args(["rev-list", "--objects", range])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git rev-list: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {stderr}"
        )));
    }

    Ok(parse_rev_list_objects_output(&output.stdout))
}

/// Parse `git ls-tree -r` output into `(blob_hash, filename)` pairs.
fn parse_ls_tree_output(output: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(output);
    let mut results = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Some((meta, filename)) = line.split_once('\t') else {
            continue;
        };

        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 3 || parts[1] != "blob" {
            continue;
        }

        results.push((parts[2].to_owned(), filename.to_owned()));
    }

    results
}

/// Parse `git rev-list --objects` output into `(object_hash, filename)` pairs.
fn parse_rev_list_objects_output(output: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(output);
    let mut results = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Some((object_hash, filename)) = line.split_once(' ') else {
            continue;
        };
        if filename.is_empty() {
            continue;
        }

        results.push((object_hash.to_owned(), filename.to_owned()));
    }

    results
}

fn read_lfs_pointer(blob_hash: &str) -> Result<Option<(LfsPointer, Vec<u8>)>> {
    let blob = cat_file_blob(blob_hash)?;

    // Skip blobs that are too large to be pointers.
    if blob.len() > 1024 {
        return Ok(None);
    }

    // Try to parse as LFS pointer; skip non-pointers silently.
    let pointer = match LfsPointer::parse(&blob) {
        Ok(p) if p.size > 0 => p,
        _ => return Ok(None),
    };

    Ok(Some((pointer, blob)))
}

/// Read a blob via `git cat-file -p`.
fn cat_file_blob(blob_hash: &str) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(["cat-file", "-p", blob_hash])
        .output()
        .map_err(|e| {
            CrabError::Internal(format!("failed to run git cat-file for {blob_hash}: {e}"))
        })?;

    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git cat-file failed for {blob_hash}"
        )));
    }

    Ok(output.stdout)
}

/// Discover the configured local LFS objects directory from the current repo.
fn discover_lfs_objects_dir() -> Result<PathBuf> {
    Ok(crate::lfs::config::LfsConfig::resolve_storage_dir(
        &std::env::current_dir().map_err(CrabError::Io)?,
    )?
    .join("objects"))
}

fn lfs_object_path(lfs_objects_dir: &Path, oid_hex: &str) -> PathBuf {
    let Some(first) = oid_hex.get(0..2) else {
        return lfs_objects_dir.join(oid_hex);
    };
    let Some(second) = oid_hex.get(2..4) else {
        return lfs_objects_dir.join(first).join(oid_hex);
    };

    lfs_objects_dir.join(first).join(second).join(oid_hex)
}

/// Read directory entries sorted by name.
fn read_dir_sorted(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| CrabError::Internal(format!("failed to read dir {}: {e}", dir.display())))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| CrabError::Internal(format!("failed to read dir entry: {e}")))?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_owned());
        }
    }

    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ls_tree_output_extracts_blob_entries() {
        let output = b"100644 blob abcdef\tfile.bin\n040000 tree 123456\tdir\n";

        let entries = parse_ls_tree_output(output);

        assert_eq!(entries, vec![("abcdef".to_owned(), "file.bin".to_owned())]);
    }

    #[test]
    fn parse_rev_list_objects_output_keeps_paths_with_spaces() {
        let output =
            b"1111111111111111111111111111111111111111\n2222222222222222222222222222222222222222 path with spaces.bin\n";

        let entries = parse_rev_list_objects_output(output);

        assert_eq!(
            entries,
            vec![(
                "2222222222222222222222222222222222222222".to_owned(),
                "path with spaces.bin".to_owned()
            )]
        );
    }

    #[test]
    fn ls_tree_range_rejects_open_ended_ranges() {
        let err = ls_tree_range("main..").unwrap_err();

        assert!(err.to_string().contains("expected A..B revision range"));
    }

    #[test]
    fn ls_tree_revision_rejects_three_dot_ranges() {
        let err = ls_tree_revision("main...feature").unwrap_err();

        assert!(err.to_string().contains("only A..B ranges are supported"));
    }

    #[test]
    fn lfs_object_path_uses_git_lfs_fanout() {
        let oid = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

        let path = lfs_object_path(Path::new(".git/lfs/objects"), oid);

        assert_eq!(
            path,
            PathBuf::from(
                ".git/lfs/objects/ab/cd/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
            )
        );
    }

    #[test]
    fn bad_object_path_uses_git_lfs_bad_dir() {
        let oid = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

        let path = bad_object_path(Path::new(".git/lfs/objects"), oid);

        assert_eq!(
            path,
            PathBuf::from(
                ".git/lfs/bad/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
            )
        );
    }

    #[test]
    fn unique_bad_object_path_adds_suffix_when_needed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oid");
        std::fs::write(&path, b"existing").unwrap();

        let unique = unique_bad_object_path(&path);

        assert_eq!(unique, dir.path().join("oid.1"));
    }
}
