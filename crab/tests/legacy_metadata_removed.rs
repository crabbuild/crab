//! CI guard: the legacy metadata/cache scaffolding must not reappear.
//!
//! Greps the crab source tree for patterns that were deleted in
//! section 5 of the SlateDB-metadata cutover and the local cache
//! SQLite consolidation. If any pattern is found in a real source,
//! config, or first-party manifest, the test fails. The check walks
//! the `src/` tree and selected Cargo files manually — no dependency on an
//! external `rg` binary.
//!
//! Patterns excluded as harmless:
//! - This test file itself (so we can name the patterns in its body).
//! - `target/` build artifacts.
//! - The `tests/` directory (other tests may reference removed types
//!   in comments — this guard is about the *shipped* crate).

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use std::fs;
use std::path::{Path, PathBuf};

/// Literal patterns that must not appear anywhere in first-party shipped code.
///
/// Each entry is an exact substring.
const FORBIDDEN_PATTERNS: &[&str] = &[
    "file-index/{hash}",
    "route_chunk_shard",
    "CHUNK_SHARD_COUNT",
    "chunk_index_db/shard_",
    "redb",
];

/// Lockfile patterns that are still precise enough to catch the removed
/// metadata layout through transitive dependencies.
///
/// `redb` stays forbidden in first-party source and manifests, but the
/// workspace lockfile can contain it through upstream Xet crates that do not
/// implement Crab's removed redb-backed metadata path.
const LOCKFILE_FORBIDDEN_PATTERNS: &[&str] = &[
    "file-index/{hash}",
    "route_chunk_shard",
    "CHUNK_SHARD_COUNT",
    "chunk_index_db/shard_",
];

/// Per-line patterns that, when they match, flag a `shard_XX` style
/// literal referring to the old 16-shard routing. A bare `shard_00`
/// is suspicious; `shard_foo` is not.
///
/// We match `shard_` followed by exactly two hex digits to stay
/// sharp — it rules out unrelated identifiers like `shard_index`.
fn line_has_shard_xx_literal(line: &str) -> bool {
    let bytes = line.as_bytes();
    let needle = b"shard_";
    let mut i = 0;
    while i + needle.len() + 2 <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let a = bytes[i + needle.len()];
            let b = bytes[i + needle.len() + 1];
            let trailing_non_ident = bytes
                .get(i + needle.len() + 2)
                .copied()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != b'_');
            if a.is_ascii_hexdigit() && b.is_ascii_hexdigit() && trailing_non_ident {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Recursively collect files under `root` that end with one of
/// `extensions`, skipping `target/` build artifacts.
fn collect_files(root: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>, extensions: &[&str]) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                // Skip build artifacts and cargo caches.
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name == "target" || name == ".git" {
                        continue;
                    }
                }
                visit(&path, out, extensions);
            } else if file_type.is_file() {
                let matches_ext = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| extensions.iter().any(|e| *e == ext));
                if matches_ext {
                    out.push(path);
                }
            }
        }
    }

    let mut out = Vec::new();
    visit(root, &mut out, extensions);
    out
}

/// A single match: file path + line number + offending line text.
#[derive(Debug)]
struct Match {
    path: PathBuf,
    line_no: usize,
    line: String,
    pattern: String,
}

fn scan_file(path: &Path, self_path: &Path) -> Vec<Match> {
    scan_file_with_patterns(path, self_path, FORBIDDEN_PATTERNS)
}

fn scan_file_with_patterns(path: &Path, self_path: &Path, patterns: &[&str]) -> Vec<Match> {
    // Exempt this file so it can name the patterns in the body.
    if path == self_path {
        return Vec::new();
    }

    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for (i, line) in content.lines().enumerate() {
        // Skip comment lines — the spec explicitly allows comments
        // that describe what was removed. A comment line is one whose
        // trimmed body begins with `//`, `///`, `//!`, `#`, or `*`
        // (the last covers C-style block continuations).
        let trimmed = line.trim_start();
        let is_comment = trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with("*")
            || trimmed.starts_with("#");
        if is_comment {
            continue;
        }

        for pat in patterns {
            if line.contains(pat) {
                hits.push(Match {
                    path: path.to_path_buf(),
                    line_no: i + 1,
                    line: line.to_string(),
                    pattern: (*pat).to_string(),
                });
            }
        }
        if line_has_shard_xx_literal(line) {
            hits.push(Match {
                path: path.to_path_buf(),
                line_no: i + 1,
                line: line.to_string(),
                pattern: "shard_XX (2-hex-digit literal)".to_string(),
            });
        }
    }
    hits
}

#[test]
fn legacy_metadata_patterns_are_absent_from_shipped_crate() {
    // `CARGO_MANIFEST_DIR` points at `crab/` during test compilation.
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let self_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("legacy_metadata_removed.rs");

    // Scan the shipped crate source only. `tests/` can reference the
    // removed types in strings/comments for other regression guards
    // without failing this check.
    let src_root = crate_root.join("src");
    let files = collect_files(&src_root, &["rs", "toml"]);

    let mut hits: Vec<Match> = Vec::new();
    for file in &files {
        hits.extend(scan_file(file, &self_path));
    }

    // Scan manifests and the workspace lockfile so forbidden metadata layout
    // identifiers cannot re-enter through config or transitive deps.
    let mut cargo_files = vec![crate_root.join("Cargo.toml")];
    if let Some(workspace_root) = crate_root.parent() {
        cargo_files.push(workspace_root.join("Cargo.toml"));
        cargo_files.push(workspace_root.join("Cargo.lock"));
    }
    cargo_files.sort();
    cargo_files.dedup();

    for cargo_file in cargo_files {
        if cargo_file.is_file() {
            let patterns =
                if cargo_file.file_name().and_then(|name| name.to_str()) == Some("Cargo.lock") {
                    LOCKFILE_FORBIDDEN_PATTERNS
                } else {
                    FORBIDDEN_PATTERNS
                };
            hits.extend(scan_file_with_patterns(&cargo_file, &self_path, patterns));
        }
    }

    if !hits.is_empty() {
        let mut report = String::from(
            "legacy metadata pattern(s) detected in shipped crate — the \
             metadata/cache cutover forbids these identifiers outside this test:\n",
        );
        for Match {
            path,
            line_no,
            line,
            pattern,
        } in &hits
        {
            report.push_str(&format!(
                "  {}:{}  [pattern: {}]\n    {}\n",
                path.display(),
                line_no,
                pattern,
                line.trim(),
            ));
        }
        panic!("{report}");
    }
}

#[test]
fn line_has_shard_xx_literal_detects_old_layout_names() {
    assert!(line_has_shard_xx_literal("let p = \"shard_0a/entries\";"));
    assert!(line_has_shard_xx_literal("shard_ff"));
    assert!(!line_has_shard_xx_literal("shard_index"));
    assert!(!line_has_shard_xx_literal("shard_"));
    assert!(!line_has_shard_xx_literal("some other text"));
    assert!(!line_has_shard_xx_literal("shard_0g"));
}
