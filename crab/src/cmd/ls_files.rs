//! `crab ls-files` — list files tracked by crab.
//!
//! Walks the working tree and reports files matching `.gitattributes`
//! patterns with `filter=crab`, showing their hydration state and
//! pointer metadata.

use std::path::Path;

use serde::Serialize;

use crate::core::error::Result;
use crate::core::output::{OutputMode, emit_json};
use crab_types::pointer::{Pointer, hex_encode};

/// Output mode for `ls-files`.
pub struct LsFilesArgs {
    /// Show full 64-char hashes instead of abbreviated 10-char.
    pub long: bool,
    /// Show file sizes in human-readable format.
    pub size: bool,
    /// Show only file names, no OID or marker.
    pub name_only: bool,
    /// Output mode resolved from CLI flags.
    pub mode: OutputMode,
    /// Show debug info (all fields).
    pub debug: bool,
}

/// Run `crab ls-files` in the current working directory.
pub fn run_ls_files(args: &LsFilesArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_ls_files_in(&cwd, args)
}

/// List crab-tracked files rooted at `root`.
pub fn run_ls_files_in(root: &Path, args: &LsFilesArgs) -> Result<()> {
    let patterns = parse_crab_patterns(root)?;
    if patterns.is_empty() {
        tracing::info!("no crab-tracked patterns in .gitattributes");
        return Ok(());
    }

    let mut entries: Vec<FileEntry> = Vec::new();
    walk_tracked_files(root, root, &patterns, &mut entries)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    if args.mode == OutputMode::Json {
        let payload = LsFilesPayload {
            files: entries.iter().map(LsFileEntry::from).collect(),
        };
        emit_json("ls-files", "1.1", payload);
    } else {
        if !args.name_only && !args.debug && !entries.is_empty() {
            print_header(args);
        }
        for entry in &entries {
            print_entry(entry, args);
        }
        if !args.name_only && !args.debug && !entries.is_empty() {
            print_legend();
        }
    }

    Ok(())
}

/// A single tracked file entry.
struct FileEntry {
    /// Relative path from the repo root.
    path: String,
    /// Whether the file is hydrated (full content) or a pointer.
    hydrated: bool,
    /// File size on disk.
    size: u64,
    /// Abbreviated hash from the pointer, if available.
    hash: Option<String>,
}

/// Parse `.gitattributes` for patterns with `filter=crab`.
fn parse_crab_patterns(root: &Path) -> Result<Vec<String>> {
    let ga_path = root.join(".gitattributes");
    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let patterns = content
        .lines()
        .filter(|line| line.contains("filter=crab"))
        .filter_map(|line| line.split_whitespace().next())
        .map(String::from)
        .collect();

    Ok(patterns)
}

/// Walk the tree collecting files that match crab patterns.
fn walk_tracked_files(
    root: &Path,
    dir: &Path,
    patterns: &[String],
    entries: &mut Vec<FileEntry>,
) -> Result<()> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        // Skip hidden directories (.git, .crab, etc.)
        if file_type.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            walk_tracked_files(root, &path, patterns, entries)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let rel = path.strip_prefix(root).unwrap_or(&path);

        if !matches_any_pattern(rel, patterns) {
            continue;
        }

        let metadata = std::fs::metadata(&path)?;
        let size = metadata.len();

        // Try to read as a pointer file (small files < 1KB).
        let (hydrated, hash) = if size < 1024 {
            match std::fs::read(&path) {
                Ok(content) => match Pointer::parse(&content) {
                    Ok(ptr) => (false, Some(hex_encode(&ptr.file_hash))),
                    Err(_) => (true, None),
                },
                Err(_) => (true, None),
            }
        } else {
            (true, None)
        };

        entries.push(FileEntry {
            path: rel.to_string_lossy().into_owned(),
            hydrated,
            size,
            hash,
        });
    }

    Ok(())
}

/// Check if a relative path matches any of the glob patterns.
fn matches_any_pattern(rel_path: &Path, patterns: &[String]) -> bool {
    let path_str = rel_path.to_string_lossy();
    for pattern in patterns {
        if pattern == "*" {
            return true;
        }
        // Simple glob matching: *.ext style
        if let Some(ext) = pattern.strip_prefix("*.")
            && path_str.ends_with(&format!(".{ext}"))
        {
            return true;
        }
        // Exact match
        if *pattern == *path_str {
            return true;
        }
        // Double-star prefix: **/*.ext
        if let Some(suffix) = pattern.strip_prefix("**/")
            && let Some(ext) = suffix.strip_prefix("*.")
            && path_str.ends_with(&format!(".{ext}"))
        {
            return true;
        }
    }
    false
}

fn print_header(args: &LsFilesArgs) {
    let hash_width = if args.long { 64 } else { 10 };
    let hash_label = "Hash";
    let status_label = "Status";
    let size_col = if args.size { "     Size  " } else { "" };

    println!("{hash_label:<hash_width$}  {status_label:<10}  {size_col}File");
    let separator_hash = "─".repeat(hash_width);
    let separator_status = "─".repeat(10);
    let separator_size = if args.size {
        format!("  {}", "─".repeat(9))
    } else {
        String::new()
    };
    println!("{separator_hash}  {separator_status}{separator_size}  ────────────────────");
}

fn print_legend() {
    println!();
    println!("Status legend:");
    println!("  pointer    — dehydrated; content stored in remote (run `crab hydrate` to restore)");
    println!("  hydrated   — full content present locally");
    println!("  unstaged   — tracked by pattern but not yet added (run `crab add` to stage)");
}

fn print_entry(entry: &FileEntry, args: &LsFilesArgs) {
    if args.debug {
        println!(
            "filepath: {}\n    size: {}\n hydrated: {}\n     oid: {}\n",
            entry.path,
            entry.size,
            entry.hydrated,
            entry.hash.as_deref().unwrap_or("<none>"),
        );
        return;
    }

    if args.name_only {
        println!("{}", entry.path);
        return;
    }

    let status = if entry.hash.is_some() && !entry.hydrated {
        "pointer"
    } else if entry.hydrated && entry.hash.is_some() {
        "hydrated"
    } else {
        "unstaged"
    };

    let hash_width = if args.long { 64 } else { 10 };
    let oid_display = match &entry.hash {
        Some(h) => {
            let len = if args.long {
                h.len().min(64)
            } else {
                10.min(h.len())
            };
            &h[..len]
        }
        None => "··········",
    };

    if args.size {
        println!(
            "{:<hash_width$}  {:<10}  {:>9}  {}",
            oid_display,
            status,
            format_bytes(entry.size),
            entry.path,
            hash_width = hash_width,
        );
    } else {
        println!(
            "{:<hash_width$}  {:<10}  {}",
            oid_display,
            status,
            entry.path,
            hash_width = hash_width,
        );
    }
}

/// Serializable payload for `--json` output. Field names match the
/// pre-envelope bare JSON that `ls-files` used to emit.
#[derive(Serialize, schemars::JsonSchema)]
pub struct LsFilesPayload {
    files: Vec<LsFileEntry>,
}

/// A single file entry in the JSON payload.
#[derive(Serialize, schemars::JsonSchema)]
pub struct LsFileEntry {
    name: String,
    size: u64,
    hydrated: bool,
    oid: Option<String>,
}

impl From<&FileEntry> for LsFileEntry {
    fn from(e: &FileEntry) -> Self {
        Self {
            name: e.path.clone(),
            size: e.size,
            hydrated: e.hydrated,
            oid: e.hash.clone(),
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_crab_patterns_extracts_filter_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n*.txt text\n",
        )
        .unwrap();

        let patterns = parse_crab_patterns(dir.path()).unwrap();
        assert_eq!(patterns, vec!["*.bin"]);
    }

    #[test]
    fn parse_crab_patterns_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let patterns = parse_crab_patterns(dir.path()).unwrap();
        assert!(patterns.is_empty());
    }

    #[test]
    fn matches_star_ext() {
        assert!(matches_any_pattern(
            Path::new("data.bin"),
            &["*.bin".into()]
        ));
        assert!(!matches_any_pattern(
            Path::new("data.txt"),
            &["*.bin".into()]
        ));
    }

    #[test]
    fn matches_double_star() {
        assert!(matches_any_pattern(
            Path::new("sub/dir/data.bin"),
            &["**/*.bin".into()]
        ));
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
    }
}
