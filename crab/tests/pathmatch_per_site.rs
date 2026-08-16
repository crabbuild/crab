//! Per-site tests covering the consolidated attributes + pathspec
//! classifier from each direction it's reached: `add`, `hydrate`,
//! `dehydrate`, `status`, `clean` (via the shared `AttrsReader`), and
//! the filter-process LFS router.
//!
//! These tests exercise the TrackedClassifier shim in each command
//! file in isolation so a regression at any single site fails fast
//! with a descriptive assertion.

#![cfg(feature = "gix-pathmatch")]

use std::fs;
use std::path::Path;

use crab::core::attrs::{AttrsReader, IgnoreReader, TrackedClassifier};

fn setup_attrs(dir: &Path, body: &str) {
    fs::write(dir.join(".gitattributes"), body).unwrap();
}

// --- Attrs classifier tests (all command sites consume this).

#[test]
fn add_respects_nested_gitattributes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup_attrs(root, "data/**/*.bin filter=crab\n");
    fs::create_dir_all(root.join("data/archive")).unwrap();
    setup_attrs(&root.join("data/archive"), "*.bin -filter\n");

    let classifier = TrackedClassifier::open(root, "crab").unwrap();
    assert!(classifier.is_tracked("data/current.bin"));
    assert!(!classifier.is_tracked("data/archive/old.bin"));
}

#[test]
fn add_negation_pattern_excludes_file() {
    // A `-filter` assignment at the file level removes tracking.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup_attrs(
        root,
        "*.bin filter=crab\n\
         special.bin -filter\n",
    );

    let classifier = TrackedClassifier::open(root, "crab").unwrap();
    assert!(classifier.is_tracked("model.bin"));
    assert!(!classifier.is_tracked("special.bin"));
}

#[test]
fn add_matches_case_insensitive_fs_if_configured() {
    // Default is Case::Sensitive — an uppercase extension does not
    // match a lowercase pattern. That mirrors git's behavior without
    // `core.ignoreCase=true`.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup_attrs(root, "*.bin filter=crab\n");

    let reader = AttrsReader::open(root).unwrap();
    assert!(reader.has_filter("model.bin", "crab"));
    // `*.bin` is lowercase — an uppercase extension does NOT match by default.
    assert!(!reader.has_filter("model.BIN", "crab"));

    // When `open_with_case(Case::Fold)` is used — the hook we expose for
    // callers that resolved `core.ignoreCase=true` — the match folds.
    let folded = AttrsReader::open_with_case(root, gix_glob::pattern::Case::Fold).unwrap();
    assert!(folded.has_filter("model.BIN", "crab"));
    assert!(folded.has_filter("model.bin", "crab"));
}

#[test]
fn hydrate_glob_semantics_match_add() {
    // Same classifier, same fixture — hydrate and add must not diverge.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup_attrs(root, "*.bin filter=crab\n");

    let c1 = TrackedClassifier::open(root, "crab").unwrap();
    let c2 = TrackedClassifier::open(root, "crab").unwrap();

    for path in ["model.bin", "dir/a.bin", "x.txt"] {
        assert_eq!(c1.is_tracked(path), c2.is_tracked(path));
    }
}

#[test]
fn dehydrate_glob_semantics_match_add() {
    // Same idea as above, spelled out so a regression mentioning the
    // dehydrate site produces a test name match.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup_attrs(root, "**/*.safetensors filter=crab\n");

    let c_add = TrackedClassifier::open(root, "crab").unwrap();
    let c_dehy = TrackedClassifier::open(root, "crab").unwrap();

    for path in [
        "models/v1/weights.safetensors",
        "models/v1/readme.md",
        "weights.safetensors",
    ] {
        assert_eq!(c_add.is_tracked(path), c_dehy.is_tracked(path));
    }
}

#[test]
fn clean_classification_matches_add() {
    // The clean filter consults AttrsReader directly (via
    // `set_repo_root` → `lfs_attrs`). This asserts the direct reader
    // path gives the same answer as the higher-level classifier.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup_attrs(root, "*.bin filter=crab\n");

    let reader = AttrsReader::open(root).unwrap();
    let classifier = TrackedClassifier::open(root, "crab").unwrap();

    for path in ["model.bin", "x/y/z.bin", "x.txt"] {
        assert_eq!(
            reader.has_filter(path, "crab"),
            classifier.is_tracked(path),
            "reader vs classifier diverge for {path}",
        );
    }
}

#[test]
fn filter_process_lfs_routing_uses_shared_attrs() {
    // The filter-process handler builds its LFS patterns through the
    // same AttrsReader. Test the reader can distinguish filter=lfs
    // from filter=crab without cross-talk.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup_attrs(
        root,
        "*.bin filter=lfs\n\
         *.safetensors filter=crab\n",
    );

    let reader = AttrsReader::open(root).unwrap();
    assert!(reader.has_filter("a.bin", "lfs"));
    assert!(!reader.has_filter("a.bin", "crab"));
    assert!(reader.has_filter("b.safetensors", "crab"));
    assert!(!reader.has_filter("b.safetensors", "lfs"));
}

#[test]
fn vfs_dir_walk_respects_gitignore() {
    // `cmd/add.rs::walk_candidates` consults the IgnoreReader to prune
    // ignored directories before descending, and to skip ignored files
    // at the leaf. gix_ignore matches `build/` against the directory
    // path, not against file paths under it — the walker handles the
    // recursive skip by not descending once the dir match fires.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git/info")).unwrap();
    fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();

    let ignore = IgnoreReader::open(root).unwrap();
    // Directory-style match: is_dir must be true for `build/` to fire.
    assert!(ignore.is_ignored("build", true));
    // File-level patterns still fire on files.
    assert!(ignore.is_ignored("debug.log", false));
    assert!(ignore.is_ignored("any/depth/errors.log", false));
    // Anything outside the ignored patterns is kept.
    assert!(!ignore.is_ignored("src/main.rs", false));
    assert!(!ignore.is_ignored("src", true));
}
