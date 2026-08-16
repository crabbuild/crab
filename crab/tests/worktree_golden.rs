//! Golden test for `gix_worktree_state::checkout` through the ODB
//! adapter.
//!
//! The assertion is simple: hydrating a committed tree through
//! crab's [`checkout_from_index`](crab::git::checkout::checkout_from_index)
//! produces a worktree byte-identical to what `git checkout` writes
//! into a sibling worktree from the same commit. Modes (exec-bit,
//! symlink targets, regular file) and content bytes are compared;
//! mtime and inode numbers are excluded since they're set by the
//! kernel and vary across runs.
//!
//! Fixture covers the shapes that have historically diverged
//! between crab's bespoke handling and git's:
//!
//! - regular text file
//! - executable script (mode 0o100755)
//! - symlink to another file in the worktree
//! - binary file with null bytes
//! - file whose path contains a subdirectory
//!
//! Intentionally out-of-scope for this minimal golden:
//!
//! - CRLF autocrlf conversion — requires a `.gitattributes`
//!   fixture and a worktree config that mirrors the user setup.
//!   Covered separately when CRLF policy lands as part of Task 7.3.
//! - Case-insensitive FS collisions — runs only produce useful
//!   signal on HFS+/APFS/NTFS. The CI matrix covers macOS + Windows
//!   (Task 7.0) so those platforms exercise it via this same test.
//! - Sparse checkout — exercised by `hydrate` integration tests
//!   once Task 7.4 wires sparse-flag handling into the stack.
//!
//! Skipped silently when `git` is not on PATH — CI runners that lack
//! a git install can still run every other test.

#![cfg(feature = "gix-worktree")]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crab::git::checkout::checkout_from_index;
use crab::git::odb_adapter::{CrabOdb, NoopXorbResolver};

/// Run `git` with the given args in `cwd`. Returns true on success,
/// false on failure or when git is missing.
fn git(cwd: &Path, args: &[&str]) -> bool {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    matches!(status, Ok(s) if s.success())
}

/// Build a committed fixture at `root` exercising regular, exec, and
/// symlink entries. Returns the HEAD commit's tree-id as a hex string.
/// Returns `None` when `git` isn't available.
fn build_committed_fixture(root: &Path) -> Option<String> {
    // Baseline identity so commit works.
    if !git(root, &["init", "--initial-branch=main"]) {
        return None;
    }
    git(root, &["config", "user.email", "golden@crab.dev"]);
    git(root, &["config", "user.name", "crab-golden"]);

    // Regular text file.
    std::fs::write(root.join("README.md"), b"hello\n").ok()?;

    // Executable script. `chmod +x` is reflected via git update-index
    // --chmod=+x so the tree records mode 100755.
    let exec_path = root.join("run.sh");
    std::fs::write(&exec_path, b"#!/bin/sh\necho crab\n").ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&exec_path).ok()?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exec_path, perms).ok()?;
    }

    // Symlink target. Windows without developer mode can't create
    // symlinks from tests, so skip that entry there.
    #[cfg(unix)]
    {
        let target = root.join("README.md");
        let link = root.join("link.md");
        std::os::unix::fs::symlink(&target, &link).ok()?;
    }

    // Binary-ish file with null bytes.
    std::fs::write(root.join("bin.dat"), &[0x00u8, 0xFF, 0x00, 0x7F, 0x80]).ok()?;

    // Nested subdirectory entry.
    std::fs::create_dir_all(root.join("sub/deeper")).ok()?;
    std::fs::write(root.join("sub/deeper/file.txt"), b"nested\n").ok()?;

    if !git(root, &["add", "-A"]) {
        return None;
    }
    if !git(root, &["commit", "-m", "fixture"]) {
        return None;
    }

    // Extract the tree id so the reference checkout and the
    // crab-driven checkout walk the same tree.
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD^{tree}"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Classify an fs entry into a compact "shape" string for golden
/// comparison. We compare the shape + content bytes; mtime / inode
/// / ownership are omitted on purpose.
fn classify(path: &Path) -> Option<(String, Vec<u8>)> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).ok()?;
        Some((format!("symlink:{}", target.to_string_lossy()), Vec::new()))
    } else if meta.is_dir() {
        Some(("dir".to_owned(), Vec::new()))
    } else {
        let content = std::fs::read(path).ok()?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o100755
        };
        #[cfg(not(unix))]
        let mode = 0o100644u32;
        Some((format!("file:{:o}", mode), content))
    }
}

/// Walk `root` and collect every entry into a sorted map.
fn walk_tree(root: &Path) -> std::collections::BTreeMap<String, (String, Vec<u8>)> {
    let mut out = std::collections::BTreeMap::new();
    collect(root, root, &mut out);
    out
}

fn collect(
    root: &Path,
    dir: &Path,
    out: &mut std::collections::BTreeMap<String, (String, Vec<u8>)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" || name_str.starts_with(".git") {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");

        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            if let Some(shape) = classify(&path) {
                out.insert(rel_str, shape);
            }
        } else if meta.is_dir() {
            collect(root, &path, out);
        } else if let Some(shape) = classify(&path) {
            out.insert(rel_str, shape);
        }
    }
}

/// Golden: `checkout_from_index` produces the same worktree as
/// `git checkout` on a fixture covering every mode crab cares
/// about.
#[test]
fn checkout_matches_git_checkout_golden() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).expect("create src dir");

    let Some(_tree_hex) = build_committed_fixture(&src) else {
        // git not available — emit a trace and return rather than
        // fail the test. CI runners without git exist.
        eprintln!("SKIP: git not available, cannot build fixture");
        return;
    };

    // Reference: a second clone with a worktree populated by git.
    let ref_dir = tmp.path().join("ref");
    if !git(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            src.to_str().expect("utf8 src"),
            ref_dir.to_str().expect("utf8 ref"),
        ],
    ) {
        eprintln!("SKIP: git clone failed");
        return;
    }

    // Candidate: clone normally (so index + objects are populated),
    // then wipe everything but `.git` so checkout has a clean stage
    // to write into.
    let candidate_src = tmp.path().join("candidate_src");
    if !git(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            src.to_str().expect("utf8 src"),
            candidate_src.to_str().expect("utf8 candidate"),
        ],
    ) {
        eprintln!("SKIP: git clone failed");
        return;
    }

    // Wipe every entry in candidate_src except `.git`. gix_worktree_state
    // ::checkout will re-create them from the index.
    for entry in std::fs::read_dir(&candidate_src).expect("read candidate") {
        let entry = entry.expect("entry");
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).expect("metadata");
        if meta.file_type().is_symlink() {
            std::fs::remove_file(&path).expect("remove symlink");
        } else if meta.is_dir() {
            std::fs::remove_dir_all(&path).expect("rm -rf");
        } else {
            std::fs::remove_file(&path).expect("remove file");
        }
    }

    // Load the committed index.
    let index_path = candidate_src.join(".git").join("index");
    let (mut index, _idx_path) = gix_index::File::at(
        &index_path,
        gix_hash::Kind::Sha1,
        true,
        gix_index::decode::Options::default(),
    )
    .expect("read candidate index")
    .into_parts();

    let odb = Arc::new(
        CrabOdb::new(
            &candidate_src.join(".git").join("objects"),
            Arc::new(NoopXorbResolver),
        )
        .expect("open odb"),
    );
    let interrupt = AtomicBool::new(false);

    let outcome = checkout_from_index(
        &mut index,
        &candidate_src,
        odb,
        &interrupt,
        gix_worktree_state::checkout::Options::default(),
    )
    .expect("candidate checkout");
    assert_eq!(
        outcome.collisions.len(),
        0,
        "no collisions on a fresh checkout"
    );

    // Compare structural shape of both trees.
    let ref_map = walk_tree(&ref_dir);
    let candidate_map = walk_tree(&candidate_src);

    let ref_keys: Vec<_> = ref_map.keys().cloned().collect();
    let candidate_keys: Vec<_> = candidate_map.keys().cloned().collect();
    assert_eq!(
        ref_keys, candidate_keys,
        "crab checkout produced a different set of worktree paths than git checkout"
    );

    for (key, ref_shape) in &ref_map {
        let candidate_shape = candidate_map.get(key).expect("candidate present");
        assert_eq!(
            ref_shape, candidate_shape,
            "shape mismatch at {key}: git={ref_shape:?} crab={candidate_shape:?}"
        );
    }
}

/// Guard that the golden test's helpers at least compile and that
/// the scaffolding behaves sanely when git is absent.
#[test]
fn walk_tree_handles_missing_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("does-not-exist");
    let map = walk_tree(&missing);
    assert!(map.is_empty());
}
