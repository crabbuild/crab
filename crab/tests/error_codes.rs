//! Golden test for the error-code catalog.
//!
//! This test snapshots every `CRAB-E####` code and its summary.
//! Adding, removing, or renumbering a code will cause this test to
//! fail until the golden file is explicitly updated.
//!
//! To update the golden file after an intentional change:
//!   CRAB_UPDATE_GOLDEN=1 cargo test --test error_codes

use crab::core::error_catalog::{ALL_CODES, lookup};

const GOLDEN_FILE: &str = "tests/golden/error_codes.txt";

fn generate_golden() -> String {
    let mut out = String::new();
    for code in ALL_CODES {
        if let Some(exp) = lookup(code) {
            out.push_str(code);
            out.push_str(": ");
            out.push_str(exp.summary);
            out.push('\n');
        }
    }
    out
}

#[test]
fn error_code_catalog_golden() {
    let actual = generate_golden();

    // If CRAB_UPDATE_GOLDEN is set, write the golden file and pass.
    if std::env::var("CRAB_UPDATE_GOLDEN").is_ok() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, &actual)
            .unwrap_or_else(|e| panic!("failed to write golden file {}: {e}", path.display()));
        return;
    }

    let golden_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN_FILE);
    let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
        panic!(
            "golden file not found at {}. Run with CRAB_UPDATE_GOLDEN=1 to create it.\n\
             Error: {e}",
            golden_path.display()
        )
    });

    if actual != expected {
        // Show a helpful diff.
        let actual_lines: Vec<&str> = actual.lines().collect();
        let expected_lines: Vec<&str> = expected.lines().collect();

        let mut diff = String::new();
        diff.push_str("Error code catalog has changed!\n\n");

        // Show added codes.
        for line in &actual_lines {
            if !expected_lines.contains(line) {
                diff.push_str(&format!("+ {line}\n"));
            }
        }
        // Show removed codes.
        for line in &expected_lines {
            if !actual_lines.contains(line) {
                diff.push_str(&format!("- {line}\n"));
            }
        }

        diff.push_str(
            "\nTo update the golden file, run:\n  \
             CRAB_UPDATE_GOLDEN=1 cargo test --test error_codes\n",
        );
        panic!("{diff}");
    }
}

#[test]
fn every_code_has_non_empty_explanation() {
    for code in ALL_CODES {
        let exp = lookup(code).unwrap_or_else(|| panic!("missing explanation for {code}"));
        assert!(!exp.summary.is_empty(), "{code} has empty summary");
        assert!(!exp.causes.is_empty(), "{code} has empty causes");
        assert!(!exp.remediation.is_empty(), "{code} has empty remediation");
    }
}
