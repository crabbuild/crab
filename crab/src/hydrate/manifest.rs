//! Manifest parser for sparse hydration.
//!
//! A manifest is a newline-delimited file of paths (or glob patterns)
//! relative to the repo root. Blank lines and comment lines (starting
//! with `#`) are skipped. Entries containing glob meta-characters
//! (`*`, `?`, `[`, `{`) are parsed as [`globset::Glob`]; everything
//! else is treated as a literal [`PathBuf`].

use std::io::BufRead;
use std::path::PathBuf;

use globset::Glob;
use tracing::debug;

use crate::core::{CrabError, Result};

/// Characters whose presence in a line signals a glob pattern rather
/// than a literal path.
const GLOB_META_CHARS: &[char] = &['*', '?', '[', '{'];

/// A single entry parsed from a manifest file.
#[derive(Debug, Clone)]
pub enum ManifestEntry {
    /// A literal file path relative to the repo root.
    Path(PathBuf),
    /// A glob pattern that will be expanded against the working tree.
    Glob(Glob),
}

/// Parse a manifest from any [`BufRead`] source (file or stdin).
///
/// Skips blank lines and comment lines (starting with `#` after
/// trimming). Lines containing glob meta-characters are parsed as
/// [`Glob`]; all others become literal [`PathBuf`] entries.
///
/// Returns [`CrabError::ManifestParse`] if a glob pattern is
/// syntactically invalid.
pub fn parse_manifest(reader: impl BufRead) -> Result<Vec<ManifestEntry>> {
    let mut entries = Vec::new();

    for (line_idx, line_result) in reader.lines().enumerate() {
        let raw_line = line_result?;
        let trimmed = raw_line.trim();

        // Skip blank lines.
        if trimmed.is_empty() {
            continue;
        }

        // Skip comment lines.
        if trimmed.starts_with('#') {
            continue;
        }

        let line_number = u32::try_from(line_idx + 1).unwrap_or(u32::MAX);

        let entry = if trimmed.contains(GLOB_META_CHARS) {
            let glob = Glob::new(trimmed).map_err(|e| CrabError::ManifestParse {
                line: line_number,
                reason: e.to_string(),
            })?;
            debug!(
                line = line_number,
                pattern = trimmed,
                "manifest: glob entry"
            );
            ManifestEntry::Glob(glob)
        } else {
            debug!(line = line_number, path = trimmed, "manifest: path entry");
            ManifestEntry::Path(PathBuf::from(trimmed))
        };

        entries.push(entry);
    }

    debug!(count = entries.len(), "manifest: parsed entries");
    Ok(entries)
}

/// Convenience: parse a manifest from a file path, or from stdin if
/// `path` is `"-"`.
pub fn parse_manifest_from(path: &str) -> Result<Vec<ManifestEntry>> {
    if path == "-" {
        let stdin = std::io::stdin();
        let reader = stdin.lock();
        parse_manifest(reader)
    } else {
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        parse_manifest(reader)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(input: &str) -> Vec<ManifestEntry> {
        parse_manifest(Cursor::new(input)).unwrap()
    }

    #[test]
    fn literal_paths() {
        let entries = parse("src/main.rs\nREADME.md\n");
        assert_eq!(entries.len(), 2);
        assert!(
            matches!(&entries[0], ManifestEntry::Path(p) if p == &PathBuf::from("src/main.rs"))
        );
        assert!(matches!(&entries[1], ManifestEntry::Path(p) if p == &PathBuf::from("README.md")));
    }

    #[test]
    fn glob_patterns() {
        let entries = parse("src/**/*.rs\n*.toml\ndata/[0-9].csv\nlib/{a,b}.rs\n");
        assert_eq!(entries.len(), 4);
        for entry in &entries {
            assert!(matches!(entry, ManifestEntry::Glob(_)));
        }
    }

    #[test]
    fn question_mark_is_glob() {
        let entries = parse("file?.txt\n");
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], ManifestEntry::Glob(_)));
    }

    #[test]
    fn skips_blank_lines() {
        let entries = parse("\n\nsrc/main.rs\n\n\n");
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], ManifestEntry::Path(_)));
    }

    #[test]
    fn skips_comment_lines() {
        let entries = parse("# This is a comment\nsrc/main.rs\n  # indented comment\nREADME.md\n");
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn mixed_entries() {
        let input = "\
# CI manifest
src/main.rs
src/**/*.rs

# Config files
*.toml
Cargo.lock
";
        let entries = parse(input);
        assert_eq!(entries.len(), 4);
        assert!(
            matches!(&entries[0], ManifestEntry::Path(p) if p == &PathBuf::from("src/main.rs"))
        );
        assert!(matches!(&entries[1], ManifestEntry::Glob(_)));
        assert!(matches!(&entries[2], ManifestEntry::Glob(_)));
        assert!(matches!(&entries[3], ManifestEntry::Path(p) if p == &PathBuf::from("Cargo.lock")));
    }

    #[test]
    fn empty_input() {
        let entries = parse("");
        assert!(entries.is_empty());
    }

    #[test]
    fn only_comments_and_blanks() {
        let entries = parse("# comment\n\n# another\n\n");
        assert!(entries.is_empty());
    }

    #[test]
    fn trims_whitespace() {
        let entries = parse("  src/main.rs  \n  *.toml  \n");
        assert_eq!(entries.len(), 2);
        assert!(
            matches!(&entries[0], ManifestEntry::Path(p) if p == &PathBuf::from("src/main.rs"))
        );
        assert!(matches!(&entries[1], ManifestEntry::Glob(_)));
    }

    #[test]
    fn invalid_glob_returns_error() {
        let result = parse_manifest(Cursor::new("[invalid\n"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, CrabError::ManifestParse { line: 1, .. }));
    }
}
