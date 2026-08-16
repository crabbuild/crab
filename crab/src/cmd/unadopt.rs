//! `crab unadopt [--pattern <glob>...]` — restore pointer files back to their
//! full hydrated content using chunks from the local staging area.
//!
//! This reverses a `crab adopt` operation before commit: reads the pointer
//! blob to identify the file hash, retrieves the CDC chunks from staging,
//! reconstructs the original content, writes it back to disk, and unstages
//! the file from the git index.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{OutputMode, emit_json};
use crate::core::pattern::{PatternFilter, build_filter};
use crate::core::style::CliStyle;
use crab_staging::StagingArea;
use crab_types::pointer::{MAX_POINTER_SIZE, Pointer, is_pointer};
use crab_xet::hash::MerkleHash;

/// Arguments for the `crab unadopt` command.
pub struct UnadoptArgs {
    /// Glob patterns to match pointer files for restoration.
    pub patterns: Vec<String>,
    /// Output mode.
    pub mode: OutputMode,
}

/// JSON output payload for unadopt results.
#[derive(Debug, Serialize)]
struct UnadoptPayload {
    restored: Vec<RestoredFile>,
    failed: Vec<String>,
    total_restored: usize,
    total_bytes: u64,
}

#[derive(Debug, Serialize)]
struct RestoredFile {
    path: String,
    size: u64,
}

/// Schema name for structured output.
pub const UNADOPT_SCHEMA: &str = "unadopt";

/// Run the `crab unadopt` command.
pub async fn run_unadopt(args: &UnadoptArgs, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;

    let cwd = std::env::current_dir()?;
    let repo_root = discover_repo_root(&cwd)?;

    if args.patterns.is_empty() {
        if !args.mode.is_machine() {
            eprintln!("No patterns specified. Use --pattern <glob> to select files to restore.");
        }
        return Ok(());
    }

    // Build a glob filter from the provided patterns.
    let filter = build_filter(&args.patterns, &[])?;

    // Find pointer files in the working tree matching the patterns.
    let pointers = find_pointer_files(&repo_root, &filter)?;

    if pointers.is_empty() {
        if !args.mode.is_machine() {
            eprintln!("No pointer files match the specified patterns.");
        }
        return Ok(());
    }

    // Open the staging area to read chunks.
    let staging_root = repo_root.join(".crab").join("staging");
    let staging = StagingArea::open(staging_root).await?;

    let mut restored: Vec<(PathBuf, u64)> = Vec::new();
    let mut failed: Vec<PathBuf> = Vec::new();

    for (path, pointer) in &pointers {
        check_cancelled(cancel)?;

        match restore_from_staging(&staging, pointer).await {
            Ok(content) => {
                let size = content.len() as u64;
                tokio::fs::write(path, &content).await.map_err(|e| {
                    CrabError::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "failed to write restored content to {}: {e}",
                            path.display()
                        ),
                    ))
                })?;
                restored.push((path.clone(), size));
                debug!(path = %path.display(), size, "restored from staging");
            }
            Err(_) => {
                failed.push(path.clone());
            }
        }
    }

    // Unstage restored files from the git index.
    if !restored.is_empty() {
        git_unstage(&repo_root, &restored)?;
    }

    // Close the staging area.
    staging.close().await?;

    // Report results.
    let style = CliStyle::resolve(args.mode);

    if args.mode == OutputMode::Json {
        let payload = UnadoptPayload {
            total_restored: restored.len(),
            total_bytes: restored.iter().map(|(_, s)| *s).sum(),
            restored: restored
                .iter()
                .map(|(p, s)| {
                    let rel = p.strip_prefix(&repo_root).unwrap_or(p);
                    RestoredFile {
                        path: rel.to_string_lossy().into_owned(),
                        size: *s,
                    }
                })
                .collect(),
            failed: failed
                .iter()
                .map(|p| {
                    let rel = p.strip_prefix(&repo_root).unwrap_or(p);
                    rel.to_string_lossy().into_owned()
                })
                .collect(),
        };
        emit_json(UNADOPT_SCHEMA, "1.0", &payload);
    } else {
        if !restored.is_empty() {
            let total_bytes: u64 = restored.iter().map(|(_, s)| *s).sum();
            eprintln!(
                "{}",
                style.ok(&format!(
                    "Restored {} file(s) ({})",
                    restored.len(),
                    format_size_human(total_bytes),
                ))
            );
            for (path, size) in &restored {
                let rel = path.strip_prefix(&repo_root).unwrap_or(path);
                eprintln!("  {} ({})", rel.display(), format_size_human(*size));
            }
        }

        if !failed.is_empty() {
            eprintln!(
                "{}",
                style.warn(&format!(
                    "Could not restore {} file(s) (chunks missing from staging):",
                    failed.len(),
                ))
            );
            for p in &failed {
                let rel = p.strip_prefix(&repo_root).unwrap_or(p);
                eprintln!("  {}", rel.display());
            }
            eprintln!("Suggestion: git checkout -- <file>");
        }
    }

    if !failed.is_empty() {
        return Err(CrabError::UnadoptChunksMissing {
            count: failed.len(),
            files: failed
                .iter()
                .map(|p| {
                    let rel = p.strip_prefix(&repo_root).unwrap_or(p);
                    rel.to_string_lossy().into_owned()
                })
                .collect(),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Pointer file discovery
// ---------------------------------------------------------------------------

/// Walk the working tree and find files that are pointer blobs matching
/// the given pattern filter.
fn find_pointer_files(repo_root: &Path, filter: &PatternFilter) -> Result<Vec<(PathBuf, Pointer)>> {
    let mut results = Vec::new();
    walk_for_pointers(repo_root, repo_root, filter, &mut results)?;
    Ok(results)
}

fn walk_for_pointers(
    root: &Path,
    dir: &Path,
    filter: &PatternFilter,
    out: &mut Vec<(PathBuf, Pointer)>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.')
                || name_str == "node_modules"
                || name_str == "target"
                || name_str == "__pycache__"
                || name_str == "venv"
            {
                continue;
            }
            walk_for_pointers(root, &path, filter, out)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_str = rel.to_string_lossy();

        if !filter.matches(&rel_str) {
            continue;
        }

        // Check if the file is a pointer blob.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };

        if meta.len() > MAX_POINTER_SIZE as u64 {
            continue;
        }

        let Ok(content) = std::fs::read(&path) else {
            continue;
        };

        if !is_pointer(&content) {
            continue;
        }

        if let Ok(pointer) = Pointer::parse(&content) {
            out.push((path, pointer));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Chunk restoration
// ---------------------------------------------------------------------------

/// Restore a file's content from the staging area by reading all chunks
/// for the file hash and concatenating them in order.
async fn restore_from_staging(staging: &StagingArea, pointer: &Pointer) -> Result<Vec<u8>> {
    let file_hash = MerkleHash::from(pointer.file_hash);

    // Get the ordered list of chunk hashes for this file.
    let chunk_hashes = staging.chunks_for_file(&file_hash)?;

    if chunk_hashes.is_empty() {
        return Err(CrabError::UnadoptChunksMissing {
            count: 1,
            files: vec![file_hash.hex()],
        });
    }

    // Read each chunk and concatenate.
    let mut content = Vec::with_capacity(pointer.size as usize);
    for chunk_hash in &chunk_hashes {
        let chunk_data = staging.get_chunk(chunk_hash).await?;
        match chunk_data {
            Some(data) => content.extend_from_slice(&data),
            None => {
                return Err(CrabError::UnadoptChunksMissing {
                    count: 1,
                    files: vec![file_hash.hex()],
                });
            }
        }
    }

    Ok(content)
}

// ---------------------------------------------------------------------------
// Git unstaging
// ---------------------------------------------------------------------------

/// Run `git reset HEAD -- <files>` to unstage the restored files.
fn git_unstage(repo_root: &Path, restored: &[(PathBuf, u64)]) -> Result<()> {
    let mut args_list: Vec<String> =
        vec!["reset".to_string(), "HEAD".to_string(), "--".to_string()];

    for (path, _) in restored {
        let rel = path.strip_prefix(repo_root).unwrap_or(path);
        args_list.push(rel.to_string_lossy().into_owned());
    }

    let output = std::process::Command::new("git")
        .args(&args_list)
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(stderr = %stderr, "git reset returned non-zero exit code");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Discover the git repository root from the current directory.
fn discover_repo_root(start: &Path) -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()?;

    if !output.status.success() {
        return Err(CrabError::Configuration {
            key: "git".into(),
            origin: "not inside a git repository".into(),
        });
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(PathBuf::from(root))
}

/// Format a byte count with human-readable units.
fn format_size_human(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    #[expect(
        clippy::cast_precision_loss,
        reason = "file sizes fit in f64 without meaningful precision loss"
    )]
    let b = bytes as f64;
    let mut idx = 0;
    let mut scaled = b;
    while scaled >= 1024.0 && idx < UNITS.len() - 1 {
        scaled /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{bytes} B")
    } else {
        format!("{scaled:.1} {unit}", unit = UNITS[idx])
    }
}
