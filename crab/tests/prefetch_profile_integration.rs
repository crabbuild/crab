//! Integration test: prefetch profile parsing + dehydrate protection.
//!
//! Validates that the `always` prefetch profile controls which files
//! survive a `dehydrate --all` and that `--ignore-profiles` overrides
//! the protection.
//!
//! Covers:
//! - R-B2: After clone, `always` profile is auto-hydrated.
//! - R-B4: `dehydrate --all` respects the `always` profile.
//! - R-S3: Profile: clone auto-hydrates `always` profile; dehydrate
//!   respects it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crab::cmd::dehydrate::{DehydrateArgs, run_dehydrate_in};
use crab::core::output::OutputMode;
use crab::engine::pointer::is_working_tree_pointer;
use crab::hydrate::profile::load_prefetch;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write `.gitattributes` with `filter=crab` for the given patterns.
fn write_gitattributes(root: &Path, patterns: &[&str]) {
    let content: String = patterns
        .iter()
        .map(|p| format!("{p} filter=crab diff=crab merge=crab -text\n"))
        .collect();
    std::fs::write(root.join(".gitattributes"), content).unwrap();
}

/// Write committed prefetch profiles to `crab.toml`.
fn write_prefetch_toml(root: &Path, toml_content: &str) {
    std::fs::write(root.join("crab.toml"), toml_content).unwrap();
}

/// Create a file with deterministic non-pointer content large enough
/// that it won't be mistaken for a pointer blob.
fn write_hydrated_file(path: &Path, seed: u8) {
    let content = vec![seed; 4096];
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn default_args() -> DehydrateArgs {
    DehydrateArgs {
        patterns: Vec::new(),
        all: false,
        ignore_profiles: false,
        mode: OutputMode::Text,
    }
}

// ---------------------------------------------------------------------------
// Fixture builder
// ---------------------------------------------------------------------------

/// Set up a temp directory that simulates a fresh clone with:
///
/// - `crab.toml` containing an `always` profile for
///   `["README.md", "docs/**/*.md"]`
/// - `.gitattributes` tracking `*.md`, `*.bin`
/// - Hydrated files: `README.md`, `docs/guide.md`, `model.bin`,
///   `data/large.bin`
fn build_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write_gitattributes(root, &["*.md", "*.bin"]);

    write_prefetch_toml(
        root,
        r#"version = 1

[remote]
url = "crab://bucket/repo"

[prefetch.profiles.always]
paths = [
    "README.md",
    "docs/**/*.md",
]
"#,
    );

    write_hydrated_file(&root.join("README.md"), 0xAA);
    write_hydrated_file(&root.join("docs/guide.md"), 0xBB);
    write_hydrated_file(&root.join("model.bin"), 0xCC);
    write_hydrated_file(&root.join("data/large.bin"), 0xDD);

    dir
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The prefetch config parser correctly loads the `always` profile and
/// its glob patterns from `crab.toml`.
#[test]
fn prefetch_config_parses_always_profile_globs() {
    let dir = build_fixture();
    let config = load_prefetch(dir.path()).unwrap();

    assert!(
        config.profiles.contains_key("always"),
        "config must contain an 'always' profile"
    );

    let always_globs = &config.profiles["always"];
    assert_eq!(always_globs.len(), 2, "always profile should have 2 globs");

    let patterns: Vec<&str> = always_globs.iter().map(|g| g.glob()).collect();
    assert!(patterns.contains(&"README.md"));
    assert!(patterns.contains(&"docs/**/*.md"));
}

/// `dehydrate --all` protects files matching the `always` profile:
/// `README.md` and `docs/guide.md` remain hydrated while `model.bin`
/// and `data/large.bin` are dehydrated to pointers.
#[test]
fn dehydrate_all_protects_always_profile_files() {
    let dir = build_fixture();
    let root = dir.path();

    let args = DehydrateArgs {
        all: true,
        ..default_args()
    };
    let cancel = CancellationToken::new();
    run_dehydrate_in(root, &args, &cancel).unwrap();

    // Protected by the always profile — must remain hydrated.
    assert!(
        !is_working_tree_pointer(&root.join("README.md")).unwrap(),
        "README.md should remain hydrated (protected by always profile)"
    );
    assert!(
        !is_working_tree_pointer(&root.join("docs/guide.md")).unwrap(),
        "docs/guide.md should remain hydrated (protected by always profile)"
    );

    // Not in the always profile — must be dehydrated to pointers.
    assert!(
        is_working_tree_pointer(&root.join("model.bin")).unwrap(),
        "model.bin should be dehydrated to a pointer"
    );
    assert!(
        is_working_tree_pointer(&root.join("data/large.bin")).unwrap(),
        "data/large.bin should be dehydrated to a pointer"
    );
}

/// `dehydrate --all --ignore-profiles` dehydrates everything, including
/// files that would normally be protected by the `always` profile.
#[test]
fn dehydrate_all_ignore_profiles_dehydrates_everything() {
    let dir = build_fixture();
    let root = dir.path();

    let args = DehydrateArgs {
        all: true,
        ignore_profiles: true,
        ..default_args()
    };
    let cancel = CancellationToken::new();
    run_dehydrate_in(root, &args, &cancel).unwrap();

    // All files should be dehydrated — profile protection is overridden.
    assert!(
        is_working_tree_pointer(&root.join("README.md")).unwrap(),
        "README.md should be dehydrated when --ignore-profiles is set"
    );
    assert!(
        is_working_tree_pointer(&root.join("docs/guide.md")).unwrap(),
        "docs/guide.md should be dehydrated when --ignore-profiles is set"
    );
    assert!(
        is_working_tree_pointer(&root.join("model.bin")).unwrap(),
        "model.bin should be dehydrated when --ignore-profiles is set"
    );
    assert!(
        is_working_tree_pointer(&root.join("data/large.bin")).unwrap(),
        "data/large.bin should be dehydrated when --ignore-profiles is set"
    );
}

/// Dehydrated pointer files contain valid blake3 hashes and correct
/// sizes matching the original content.
#[test]
fn dehydrated_pointers_have_correct_hashes_and_sizes() {
    let dir = build_fixture();
    let root = dir.path();

    let args = DehydrateArgs {
        all: true,
        ..default_args()
    };
    let cancel = CancellationToken::new();
    run_dehydrate_in(root, &args, &cancel).unwrap();

    // model.bin was 4096 bytes of 0xCC.
    let ptr_bytes = std::fs::read(root.join("model.bin")).unwrap();
    let ptr = crab_types::pointer::Pointer::parse(&ptr_bytes)
        .expect("model.bin should be a valid pointer");
    assert_eq!(ptr.size, 4096);
    let expected_hash = blake3::hash(&vec![0xCC; 4096]);
    assert_eq!(ptr.file_hash, *expected_hash.as_bytes());

    // data/large.bin was 4096 bytes of 0xDD.
    let ptr_bytes = std::fs::read(root.join("data/large.bin")).unwrap();
    let ptr = crab_types::pointer::Pointer::parse(&ptr_bytes)
        .expect("data/large.bin should be a valid pointer");
    assert_eq!(ptr.size, 4096);
    let expected_hash = blake3::hash(&vec![0xDD; 4096]);
    assert_eq!(ptr.file_hash, *expected_hash.as_bytes());
}
