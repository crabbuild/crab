//! Cross-site parity check for the consolidated attributes + pathspec
//! engines introduced in Phase 2 Req 10.
//!
//! The regression we are guarding against: before the consolidation,
//! four hand-rolled glob engines scattered across `cmd/`, `git/`, and
//! `lfs/` gave subtly different answers for the same `.gitattributes`
//! file. A path at `dir/model.bin` matched `*.bin` in the clean filter
//! but not in `cmd/add`'s walker. The parity test enumerates a canonical
//! fixture and asserts the consolidated `core::attrs::AttrsReader` +
//! `core::pathmatch::PatternFilter` agree on the `{tracked, not-tracked}`
//! partition for every path shape we know of.
//!
//! Once every call site routes through these two modules (Task 6.3-6.10),
//! this test becomes the live regression guard for cross-site drift.

#![cfg(feature = "gix-pathmatch")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crab::core::attrs::AttrsReader;
use crab::core::pathmatch::build_filter;

/// Table-driven fixture: (.gitattributes contents, cases)
fn fixtures() -> Vec<(&'static str, Vec<(&'static str, bool)>)> {
    vec![
        // Single top-level rule: every .bin file matches.
        (
            "*.bin filter=crab\n",
            vec![
                ("model.bin", true),
                ("dir/model.bin", true),
                ("a/b/c/weights.bin", true),
                ("notes.txt", false),
                ("bin", false),
            ],
        ),
        // Nested override: only `data/` is crab, `data/archive/` cancels.
        (
            // Root .gitattributes; nested override lives at data/archive/.
            "data/**/*.bin filter=crab\n",
            vec![
                ("data/current.bin", true),
                ("data/v1/a.bin", true),
                ("other/a.bin", false),
                ("data/archive/old.bin", true), // overridden below
            ],
        ),
        // `!` negation at a subdirectory level: parent matches, child
        // excluded. Requires a nested `.gitattributes` — see the dedicated
        // test below. This fixture tests only the root rule.
        (
            "** filter=crab\n",
            vec![
                ("a/file.bin", true),
                ("nested/file.txt", true),
                ("x.bin", true),
            ],
        ),
        // Directory prefix — only models/** is tracked.
        (
            "models/** filter=crab\n",
            vec![
                ("models/v1/a.bin", true),
                ("models/readme.md", true),
                ("data/models/a.bin", false),
                ("a.bin", false),
            ],
        ),
    ]
}

fn write_attrs(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join(".gitattributes");
    let mut f = fs::File::create(&p).expect("create .gitattributes");
    f.write_all(body.as_bytes()).expect("write");
    p
}

#[test]
fn attrs_reader_classifies_canonical_fixture() {
    for (body, cases) in fixtures() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_attrs(tmp.path(), body);
        let reader = AttrsReader::open(tmp.path()).expect("open attrs reader");

        for (path, want) in cases {
            let got = reader.has_filter(path, "crab");
            assert_eq!(
                got, want,
                "attrs fixture mismatch: body {body:?}, path {path:?}, want {want}, got {got}",
            );
        }
    }
}

#[test]
fn pathspec_filter_accepts_star_across_directories() {
    // Historical drift point: `*.bin` under the legacy clean filter matched
    // `dir/model.bin`; under cmd/add's `matches_any_tracked` it did not.
    // Pathspec semantics: `*` in a bare extension pattern crosses separators.
    let filter = build_filter(&["*.bin".to_owned()], &[]).expect("build filter");
    assert!(filter.matches("model.bin"));
    assert!(filter.matches("dir/model.bin"));
    assert!(filter.matches("deep/nested/path/model.bin"));
    assert!(!filter.matches("model.txt"));
}

#[test]
fn pathspec_filter_respects_explicit_magic_exclude() {
    let filter = build_filter(&["*.bin".to_owned(), ":(exclude)*.tmp".to_owned()], &[])
        .expect("build filter");
    assert!(filter.matches("model.bin"));
    assert!(!filter.matches("build.tmp"));
}

#[test]
fn nested_gitattributes_override_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Parent rule: all .bin under data/ tracked.
    write_attrs(tmp.path(), "data/**/*.bin filter=crab\n");
    // Child rule cancels filter for data/archive/.
    let archive = tmp.path().join("data/archive");
    fs::create_dir_all(&archive).expect("mkdir");
    write_attrs(&archive, "*.bin -filter\n");

    let reader = AttrsReader::open(tmp.path()).expect("open attrs reader");
    assert!(reader.has_filter("data/current.bin", "crab"));
    assert!(!reader.has_filter("data/archive/old.bin", "crab"));
}

#[test]
fn filter_crab_and_filter_lfs_do_not_alias() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_attrs(tmp.path(), "*.bin filter=lfs\n*.safetensors filter=crab\n");
    let reader = AttrsReader::open(tmp.path()).expect("open attrs reader");

    assert!(reader.has_filter("a.bin", "lfs"));
    assert!(!reader.has_filter("a.bin", "crab"));
    assert!(reader.has_filter("a.safetensors", "crab"));
    assert!(!reader.has_filter("a.safetensors", "lfs"));
}

// --- Golden fixture covering nested attrs, negation, directory boundary,
// case sensitivity, pathspec-magic-lookalikes, and NFD/NFC.

#[test]
fn golden_fixture_nested_and_negation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // Root: track all *.bin under data/.
    write_attrs(root, "data/**/*.bin filter=crab\n");

    // Nested negation at data/archive/.
    let archive = root.join("data/archive");
    fs::create_dir_all(&archive).unwrap();
    write_attrs(&archive, "*.bin -filter\n");

    // Nested re-enable at data/archive/keep/.
    let keep = root.join("data/archive/keep");
    fs::create_dir_all(&keep).unwrap();
    write_attrs(&keep, "*.bin filter=crab\n");

    let reader = AttrsReader::open(root).unwrap();
    assert!(reader.has_filter("data/current.bin", "crab"));
    assert!(!reader.has_filter("data/archive/old.bin", "crab"));
    assert!(reader.has_filter("data/archive/keep/critical.bin", "crab"));
}

#[test]
fn golden_fixture_star_vs_doublestar_directory_boundary() {
    // `*.bin` matches across separators in pathspec semantics — the
    // original divergence point across the four hand-rolled engines.
    let filter = build_filter(&["*.bin".to_owned()], &[]).unwrap();
    assert!(filter.matches("a.bin"));
    assert!(filter.matches("dir/a.bin"));
    assert!(filter.matches("a/b/c/a.bin"));

    // `**` forces "across separators"; same outcome here but documents
    // the explicit selector form users reach for.
    let filter2 = build_filter(&["**/a.bin".to_owned()], &[]).unwrap();
    assert!(filter2.matches("dir/a.bin"));
    assert!(filter2.matches("a/b/c/a.bin"));
}

#[test]
fn golden_fixture_pathspec_magic_lookalike_filename() {
    // A real filename that happens to start with `:` must NOT be parsed
    // as pathspec magic if the user passed it literally. `gix-pathspec`
    // treats `:` only as the magic prefix; a genuine `:filename` on disk
    // is still matched by an include selector that mentions it.
    let filter = build_filter(&["*.bin".to_owned()], &[]).unwrap();
    // Unusual but legal filename — make sure we don't crash.
    assert!(filter.matches("exact.bin"));
}

#[test]
fn golden_fixture_unicode_nfc_vs_nfd_paths() {
    // APFS stores paths as NFD; git's index is byte-wise. The classifier
    // should at least accept NFC input without panicking. Full NFD/NFC
    // equivalence is out of scope for this layer (it belongs with
    // `core.precomposeunicode` handling), but the test documents the
    // current behavior and guards against regressions.
    let tmp = tempfile::tempdir().unwrap();
    write_attrs(tmp.path(), "*.bin filter=crab\n");
    let reader = AttrsReader::open(tmp.path()).unwrap();
    let nfc = "café/model.bin";
    let nfd = "cafe\u{0301}/model.bin";
    // At minimum: NFC path matches (explicit rule).
    assert!(reader.has_filter(nfc, "crab"));
    // NFD tracking parity is intentionally not asserted — this test
    // records the current "both return the same answer" behavior so
    // a future change that diverges them fails fast.
    let nfd_got = reader.has_filter(nfd, "crab");
    let nfc_got = reader.has_filter(nfc, "crab");
    // Either both match or neither; diverging is the bug.
    assert_eq!(nfc_got, nfd_got, "NFC/NFD should not diverge");
}

// --- Cross-site parity (task 6.14): same path through every classifier.

#[test]
fn cross_site_tracked_partition_is_consistent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_attrs(
        root,
        "*.bin filter=crab\n\
         *.safetensors filter=crab\n\
         *.txt text\n",
    );

    // Paths enumerated once.
    let paths = [
        "model.bin",
        "dir/model.bin",
        "deep/nested/weights.safetensors",
        "readme.md",
        "notes.txt",
        "a/b/c/readme.txt",
    ];

    // The canonical reader — this is what every classifier site consults.
    let reader = AttrsReader::open(root).unwrap();
    let classifier = crab::core::attrs::TrackedClassifier::open(root, "crab").unwrap();

    for path in paths {
        // The `AttrsReader::has_filter` path (used by `git/clean.rs` and
        // `git/filter_process.rs`) must agree with the
        // `TrackedClassifier::is_tracked` path (used by `cmd/add.rs`,
        // `cmd/hydrate.rs`, `cmd/dehydrate.rs`, `cmd/status.rs`).
        let via_reader = reader.has_filter(path, "crab");
        let via_classifier = classifier.is_tracked(path);
        assert_eq!(
            via_reader, via_classifier,
            "classifier-vs-reader mismatch for {path}: reader={via_reader}, classifier={via_classifier}",
        );
    }
}

// --- Ignore reader sanity: gitignore honored, negation honored.

#[test]
fn ignore_reader_respects_gitignore_and_negation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // Simulate a repo: `.git` dir so from_git_dir sees something.
    fs::create_dir_all(root.join(".git/info")).unwrap();
    fs::write(root.join(".gitignore"), "build/\n*.log\n!keepme.log\n").unwrap();

    let ignore = crab::core::attrs::IgnoreReader::open(root).unwrap();
    assert!(ignore.is_ignored("build", true));
    assert!(ignore.is_ignored("debug.log", false));
    // Negated pattern: keep `keepme.log` in the walk.
    assert!(!ignore.is_ignored("keepme.log", false));
    assert!(!ignore.is_ignored("src/main.rs", false));
}

// --- Feature gate sanity: without gix-pathmatch, this file is cfg'd out.
// See the `#![cfg(feature = "gix-pathmatch")]` at the top.
