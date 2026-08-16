//! Source-level lint: forbid direct JSON-to-stdout patterns in `cmd/` modules.
//!
//! All structured output must go through `core::output` helpers (`emit_json`,
//! `JsonlStream`). This test catches two bypass patterns:
//!
//! 1. `serde_json::to_writer(stdout` — writing JSON directly to stdout
//! 2. `serde_json::to_string` near `println!` — serialize-then-print
//!
//! Plain `println!`/`print!` are allowed (text-mode output uses them
//! legitimately). `eprintln!`/`eprint!` are allowed (stderr).

use std::path::{Path, PathBuf};

/// Collect all `.rs` files under a directory, recursively.
fn collect_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return files,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(collect_rs_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

/// A single violation found by the lint.
struct Violation {
    file: PathBuf,
    line_number: usize,
    line: String,
    reason: &'static str,
}

/// Scan a source file for forbidden patterns.
///
/// Returns violations for:
/// - `serde_json::to_writer(stdout` on any line (outside `#[cfg(test)]` blocks)
/// - `serde_json::to_string` within 3 lines of a `println!` (serialize-then-print)
fn scan_file(path: &Path) -> Vec<Violation> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let lines: Vec<&str> = content.lines().collect();
    let mut violations = Vec::new();
    let mut in_test_block = false;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Skip test modules — they may legitimately use these patterns.
        if trimmed == "#[cfg(test)]" {
            in_test_block = true;
            continue;
        }
        if in_test_block {
            continue;
        }

        // Pattern 1: serde_json::to_writer(stdout...
        if trimmed.contains("serde_json::to_writer") && trimmed.contains("stdout") {
            violations.push(Violation {
                file: path.to_path_buf(),
                line_number: i + 1,
                line: line.to_string(),
                reason: "serde_json::to_writer(stdout) bypasses emit_json helper",
            });
        }

        // Pattern 2: serde_json::to_string near println!
        // Look for serde_json::to_string on this line, then check ±3 lines for println!
        if trimmed.contains("serde_json::to_string") {
            let window_start = i.saturating_sub(3);
            let window_end = (i + 4).min(lines.len());
            for j in window_start..window_end {
                if lines[j].contains("println!") {
                    violations.push(Violation {
                        file: path.to_path_buf(),
                        line_number: i + 1,
                        line: line.to_string(),
                        reason: "serde_json::to_string near println! bypasses emit_json helper",
                    });
                    break;
                }
            }
        }
    }

    violations
}

#[test]
fn no_direct_json_stdout_in_cmd_modules() {
    let cmd_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd");
    assert!(
        cmd_dir.is_dir(),
        "cmd directory not found: {}",
        cmd_dir.display()
    );

    let files = collect_rs_files(&cmd_dir);
    assert!(
        !files.is_empty(),
        "no .rs files found in {}",
        cmd_dir.display()
    );

    let mut all_violations = Vec::new();
    for file in &files {
        all_violations.extend(scan_file(file));
    }

    if !all_violations.is_empty() {
        let mut msg = String::from(
            "Found direct JSON-to-stdout patterns that bypass core::output helpers:\n\n",
        );
        for v in &all_violations {
            let rel = v
                .file
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .unwrap_or(&v.file);
            msg.push_str(&format!(
                "  {}:{}: {}\n    {}\n\n",
                rel.display(),
                v.line_number,
                v.reason,
                v.line.trim(),
            ));
        }
        msg.push_str("Use emit_json() or JsonlStream instead of writing JSON directly to stdout.");
        panic!("{msg}");
    }
}
