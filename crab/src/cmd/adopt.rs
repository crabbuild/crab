//! `crab adopt [--pattern <glob>...] [--dry-run]` — convert large files to
//! crab pointers in the current working tree.
//!
//! Walks the working tree for files matching the given patterns (or
//! auto-detected patterns from `.crab.toml` / large-file scan), replaces
//! their content with pointer blobs, stages the chunks via the same
//! pipeline as `crab add`, and updates `.gitattributes`.
//!
//! Two modes:
//! - **Dry-run** (`--dry-run`): list files that would be converted with sizes.
//! - **HEAD-only** (default): convert files in the working tree + index only.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{OutputMode, emit_json};
use crate::core::pattern::{PatternFilter, build_filter};
use crate::core::project_config::ProjectConfig;
use crate::core::style::CliStyle;
use crab_staging::StagingArea;
use crab_types::pointer::{Pointer, is_pointer};

/// Arguments for the `crab adopt` command.
pub struct AdoptArgs {
    /// Glob patterns to match (e.g. `*.bin`, `*.safetensors`).
    pub patterns: Vec<String>,
    /// Rewrite git history (requires `--force`). Currently unimplemented.
    pub rewrite_history: bool,
    /// Required with `--rewrite-history`.
    pub force: bool,
    /// Show what would be converted without making changes.
    pub dry_run: bool,
    /// Maximum number of concurrent file-processing tasks.
    pub jobs: usize,
    /// Output mode.
    pub mode: OutputMode,
    /// Show candidate files and prompt for confirmation before converting.
    pub interactive: bool,
}

/// A file candidate for adoption.
#[derive(Debug, Serialize)]
struct AdoptCandidate {
    path: String,
    size: u64,
    extension: String,
}

/// JSON output for dry-run mode.
#[derive(Debug, Serialize)]
struct DryRunOutput {
    files: Vec<AdoptCandidate>,
    total_files: usize,
    total_bytes: u64,
    total_human: String,
}

/// Run the `crab adopt` command.
pub async fn run_adopt(args: &AdoptArgs, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;

    // History rewrite mode: validate guards, then return "not yet implemented".
    // This is a stretch goal — the HEAD-only mode covers the primary use case.
    if args.rewrite_history {
        // Guard: --force is required for history rewrite.
        if !args.force {
            return Err(CrabError::Configuration {
                key: "rewrite-history".into(),
                origin: "--rewrite-history requires --force. This rewrites git history and requires force-push.".into(),
            });
        }

        // Guard: working tree must be clean.
        let status_output = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .output()
            .map_err(|e| {
                CrabError::Io(std::io::Error::new(e.kind(), format!("git status: {e}")))
            })?;
        let status_text = String::from_utf8_lossy(&status_output.stdout);
        if !status_text.trim().is_empty() {
            return Err(CrabError::Configuration {
                key: "rewrite-history".into(),
                origin: "Working tree has uncommitted changes. Commit or stash before rewriting history.".into(),
            });
        }

        // Guard: git-filter-repo must be installed.
        let filter_repo_check = std::process::Command::new("which")
            .arg("git-filter-repo")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match filter_repo_check {
            Ok(s) if s.success() => {}
            _ => {
                eprintln!("git-filter-repo is not installed.");
                eprintln!("Install: pip install git-filter-repo");
                eprintln!(
                    "Or use the default HEAD-only mode: crab adopt (without --rewrite-history)"
                );
                return Err(CrabError::Configuration {
                    key: "rewrite-history".into(),
                    origin: "git-filter-repo not found. Install it or use HEAD-only mode (crab adopt without --rewrite-history).".into(),
                });
            }
        }

        // TODO(stretch): Implement history rewrite using git-filter-repo --blob-callback.
        // The HEAD-only mode (default) covers 90%+ of use cases. History rewrite
        // would replace matching blobs across all commits with pointer content.
        return Err(CrabError::Configuration {
            key: "rewrite-history".into(),
            origin: "--rewrite-history is not yet implemented. Use the default HEAD-only mode (crab adopt without --rewrite-history).".into(),
        });
    }

    let cwd = std::env::current_dir()?;

    // Discover the git repo root.
    let repo_root = discover_repo_root(&cwd)?;

    // Resolve patterns: CLI args → .crab.toml [track] → auto-detect.
    let patterns = resolve_patterns(&args.patterns, &repo_root);

    if patterns.is_empty() {
        if !args.mode.is_machine() {
            eprintln!("No patterns to adopt. Specify --pattern or configure [track] in .crab.toml");
        }
        return Ok(());
    }

    // Build a glob filter from the resolved patterns.
    let filter = build_filter(&patterns, &[])?;

    // Walk the working tree and collect matching files.
    let candidates = collect_adopt_candidates(&repo_root, &repo_root, &filter)?;

    if candidates.is_empty() {
        if !args.mode.is_machine() {
            eprintln!("No files match the resolved patterns.");
        }
        return Ok(());
    }

    // Interactive mode: when Json + interactive, behave as dry-run.
    if args.interactive && args.mode == OutputMode::Json {
        run_dry_run(&candidates, args.mode);
        return Ok(());
    }

    // Interactive mode: display candidates and prompt for confirmation.
    if args.interactive && !args.mode.is_machine() {
        let style = CliStyle::resolve(args.mode);
        display_interactive_candidates(&candidates, &style);
        if !prompt_confirmation()? {
            return Ok(());
        }
    }

    if args.dry_run {
        run_dry_run(&candidates, args.mode);
        return Ok(());
    }

    // HEAD-only mode: convert files in place.
    run_head_only(&candidates, &repo_root, &patterns, args, cancel).await
}

// ---------------------------------------------------------------------------
// Pattern resolution
// ---------------------------------------------------------------------------

/// Resolve patterns from args, .crab.toml, or auto-detection.
fn resolve_patterns(cli_patterns: &[String], repo_root: &Path) -> Vec<String> {
    // 1. CLI patterns take priority.
    if !cli_patterns.is_empty() {
        return cli_patterns.to_vec();
    }

    // 2. Try .crab.toml [track] patterns.
    if let Some(config) = ProjectConfig::discover(repo_root)
        && let Some(track) = config.track
        && !track.patterns.is_empty()
    {
        return track.patterns;
    }

    // 3. Auto-detect by scanning for large files (same logic as init).
    auto_detect_patterns(repo_root)
}

/// Scan the working tree for large files and return glob patterns for
/// their extensions. Reuses the same heuristics as `auto_track_large_files`
/// in init.rs but without writing to `.gitattributes`.
fn auto_detect_patterns(root: &Path) -> Vec<String> {
    use std::collections::HashSet;

    let ga_path = root.join(".gitattributes");
    let existing_content = std::fs::read_to_string(&ga_path).unwrap_or_default();
    let already_tracked: HashSet<String> = existing_content
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#') && t.contains("filter=crab")
        })
        .filter_map(|line| line.split_whitespace().next().map(String::from))
        .collect();

    let mut new_exts: BTreeSet<String> = BTreeSet::new();
    let _ = scan_large_files(root, &already_tracked, &mut new_exts);

    let mut patterns: Vec<String> = already_tracked.into_iter().collect();
    for ext in new_exts {
        patterns.push(format!("*.{ext}"));
    }
    patterns.sort();
    patterns
}

/// Size threshold for auto-detection (1 MiB).
const AUTO_TRACK_SIZE_THRESHOLD: u64 = 1_048_576;

/// Well-known large-file extensions.
const WELL_KNOWN_LARGE_EXTENSIONS: &[&str] = &[
    "safetensors",
    "bin",
    "onnx",
    "pt",
    "pth",
    "h5",
    "hdf5",
    "pkl",
    "parquet",
    "arrow",
    "feather",
    "npy",
    "npz",
    "zarr",
    "fbx",
    "blend",
    "psd",
    "tiff",
    "exr",
    "dpx",
    "mov",
    "mp4",
    "avi",
    "mkv",
    "wav",
    "flac",
    "db",
    "sqlite",
    "sqlite3",
    "tar",
    "gz",
    "zip",
    "zst",
    "lz4",
];

/// Walk the tree looking for large files (mirrors init.rs logic).
fn scan_large_files(
    dir: &Path,
    already_tracked: &std::collections::HashSet<String>,
    new_exts: &mut BTreeSet<String>,
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
            scan_large_files(&path, already_tracked, new_exts)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_lowercase(),
            None => continue,
        };

        let glob = format!("*.{ext}");
        if already_tracked.contains(&glob) {
            continue;
        }

        let is_well_known = WELL_KNOWN_LARGE_EXTENSIONS.contains(&ext.as_str());
        let is_large = if is_well_known {
            true
        } else {
            match std::fs::metadata(&path) {
                Ok(m) => m.len() >= AUTO_TRACK_SIZE_THRESHOLD,
                Err(_) => false,
            }
        };

        if is_large {
            new_exts.insert(ext);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Candidate collection
// ---------------------------------------------------------------------------

/// Walk the working tree and collect files matching the pattern filter.
fn collect_adopt_candidates(
    root: &Path,
    dir: &Path,
    filter: &PatternFilter,
) -> Result<Vec<(PathBuf, u64)>> {
    let mut candidates = Vec::new();
    walk_for_candidates(root, dir, filter, &mut candidates)?;
    Ok(candidates)
}

fn walk_for_candidates(
    root: &Path,
    dir: &Path,
    filter: &PatternFilter,
    out: &mut Vec<(PathBuf, u64)>,
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
            walk_for_candidates(root, &path, filter, out)?;
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

        // Skip files that are already pointer blobs.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };

        // Could be a pointer, so check content before adopting it.
        if meta.len() <= crab_types::pointer::MAX_POINTER_SIZE as u64
            && let Ok(content) = std::fs::read(&path)
            && is_pointer(&content)
        {
            continue;
        }

        out.push((path, meta.len()));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Interactive confirmation
// ---------------------------------------------------------------------------

/// Display candidate files with colored output for interactive confirmation.
fn display_interactive_candidates(candidates: &[(PathBuf, u64)], style: &CliStyle) {
    let mut sorted: Vec<&(PathBuf, u64)> = candidates.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    let total_bytes: u64 = sorted.iter().map(|(_, s)| *s).sum();
    let n = sorted.len();

    eprintln!();
    eprintln!(
        "{}",
        style.warn(&format!(
            "{n} file(s) will be converted to crab pointers ({}):",
            format_size_human(total_bytes)
        ))
    );
    eprintln!();

    for (path, size) in &sorted {
        let display_path = path.to_string_lossy();
        let truncated = if display_path.len() > 60 {
            format!("…{}", &display_path[display_path.len() - 59..])
        } else {
            display_path.into_owned()
        };
        let size_str = format_size_human(*size);
        if style.is_enabled() {
            eprintln!(
                "  {} {}",
                truncated,
                style.dim.apply_to(format!("({size_str})"))
            );
        } else {
            eprintln!("  {truncated} ({size_str})");
        }
    }
    eprintln!();
}

/// Prompt the user for yes/no confirmation. Returns `true` if the user
/// confirms, `false` otherwise. Skips the prompt (returns `false`) when
/// stdin is not a TTY.
fn prompt_confirmation() -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }

    eprint!("Proceed? [y/N] ");
    std::io::stderr()
        .flush()
        .map_err(|e| CrabError::Io(std::io::Error::new(e.kind(), format!("flush stderr: {e}"))))?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).map_err(|e| {
        CrabError::Io(std::io::Error::new(
            e.kind(),
            format!("read confirmation: {e}"),
        ))
    })?;

    let answer = input.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

// ---------------------------------------------------------------------------
// Dry-run mode (tasks 15.1–15.5)
// ---------------------------------------------------------------------------

fn run_dry_run(candidates: &[(PathBuf, u64)], mode: OutputMode) {
    // Sort by size descending for display.
    let mut sorted: Vec<&(PathBuf, u64)> = candidates.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    let total_bytes: u64 = sorted.iter().map(|(_, s)| *s).sum();
    let n = sorted.len();

    if mode == OutputMode::Json {
        let files: Vec<AdoptCandidate> = sorted
            .iter()
            .map(|(path, size)| {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_string();
                let rel = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                AdoptCandidate {
                    path: rel,
                    size: *size,
                    extension: ext,
                }
            })
            .collect();
        let output = DryRunOutput {
            total_files: n,
            total_bytes,
            total_human: format_size_human(total_bytes),
            files,
        };
        emit_json("adopt.dry-run", "1.0", &output);
        return;
    }

    // Print table header.
    eprintln!("{:<60} {:>12} {:<10}", "PATH", "SIZE", "EXT");
    eprintln!("{}", "-".repeat(84));

    for (path, size) in &sorted {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let display_path = path.to_string_lossy();
        // Truncate long paths for readability.
        let truncated = if display_path.len() > 58 {
            format!("…{}", &display_path[display_path.len() - 57..])
        } else {
            display_path.into_owned()
        };
        eprintln!(
            "{:<60} {:>12} {:<10}",
            truncated,
            format_size_human(*size),
            ext,
        );
    }

    eprintln!();
    eprintln!(
        "Would convert {} files ({} total)",
        n,
        format_size_human(total_bytes),
    );
}

// ---------------------------------------------------------------------------
// HEAD-only mode (tasks 16.1–16.5)
// ---------------------------------------------------------------------------

async fn run_head_only(
    candidates: &[(PathBuf, u64)],
    repo_root: &Path,
    _patterns: &[String],
    args: &AdoptArgs,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;

    // Open the staging area.
    let staging_root = repo_root.join(".crab").join("staging");
    let staging = Arc::new(StagingArea::open(staging_root).await?);

    let mut converted_paths: Vec<PathBuf> = Vec::new();
    let mut new_extensions: BTreeSet<String> = BTreeSet::new();
    let mut total_original_size: u64 = 0;
    let mut total_pointer_size: u64 = 0;

    info!(
        files = candidates.len(),
        jobs = args.jobs,
        "starting adopt HEAD-only conversion"
    );

    for (abs_path, _) in candidates {
        check_cancelled(cancel)?;

        let rel_path = abs_path.strip_prefix(repo_root).unwrap_or(abs_path);
        let staged = crate::cmd::stream_stage::stage_file_streaming(
            abs_path,
            repo_root,
            &staging,
            crate::cmd::stream_stage::StreamStageProgress::default(),
            cancel,
        )
        .await?;

        total_original_size += staged.size;

        // Generate pointer blob (same format as crab add).
        let pointer = Pointer {
            file_hash: staged.file_hash,
            size: staged.size,
            shard_hint: None,
        };
        let pointer_bytes = pointer.serialize();
        total_pointer_size += pointer_bytes.len() as u64;

        // Write pointer blob to the file path, replacing original content.
        tokio::fs::write(abs_path, &pointer_bytes)
            .await
            .map_err(|e| {
                CrabError::Io(std::io::Error::new(
                    e.kind(),
                    format!("failed to write pointer to {}: {e}", abs_path.display()),
                ))
            })?;

        // Track the extension for .gitattributes update.
        if let Some(ext) = abs_path.extension().and_then(|e| e.to_str()) {
            new_extensions.insert(ext.to_lowercase());
        }

        converted_paths.push(abs_path.clone());

        debug!(
            path = %rel_path.display(),
            size = staged.size,
            chunks = staged.chunks,
            "converted to pointer"
        );
    }

    // Close the staging area to flush pending chunks.
    match Arc::try_unwrap(staging) {
        Ok(s) => s.close().await?,
        Err(_) => {
            warn!("staging area still referenced, skipping explicit close");
        }
    }

    // Update .gitattributes with new extensions.
    let ga_updated = update_gitattributes(repo_root, &new_extensions)?;

    // Run `git add` on all converted file paths + .gitattributes.
    git_add_files(repo_root, &converted_paths, ga_updated)?;

    // Print summary.
    let n = converted_paths.len();
    if !args.mode.is_machine() {
        eprintln!(
            "Converted {} files ({} → {} pointers). Staged for commit.",
            n,
            format_size_human(total_original_size),
            format_size_human(total_pointer_size),
        );
        eprintln!("Review: git diff --cached | Commit: crab ship -m 'adopt large files'");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Update `.gitattributes` with filter rules for new extensions.
/// Returns true if the file was modified.
fn update_gitattributes(root: &Path, extensions: &BTreeSet<String>) -> Result<bool> {
    let ga_path = root.join(".gitattributes");
    let existing_content = std::fs::read_to_string(&ga_path).unwrap_or_default();

    // Parse already-tracked patterns.
    let already_tracked: std::collections::HashSet<String> = existing_content
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#') && t.contains("filter=crab")
        })
        .filter_map(|line| line.split_whitespace().next().map(String::from))
        .collect();

    // Find extensions not yet tracked.
    let mut new_rules = Vec::new();
    for ext in extensions {
        let glob = format!("*.{ext}");
        if !already_tracked.contains(&glob) {
            new_rules.push(glob);
        }
    }

    if new_rules.is_empty() {
        return Ok(false);
    }

    // Append new tracking rules.
    let mut content = existing_content;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    for rule in &new_rules {
        let _ = writeln!(content, "{rule} filter=crab diff=crab merge=crab -text");
    }
    std::fs::write(&ga_path, &content)?;

    info!(rules = ?new_rules, "updated .gitattributes with new tracking rules");
    Ok(true)
}

/// Run `git add` on converted files and .gitattributes.
fn git_add_files(root: &Path, paths: &[PathBuf], include_gitattributes: bool) -> Result<()> {
    use std::process::Command;

    let mut args_list: Vec<String> = vec!["add".to_string(), "--".to_string()];

    for path in paths {
        let rel = path.strip_prefix(root).unwrap_or(path);
        args_list.push(rel.to_string_lossy().into_owned());
    }

    if include_gitattributes {
        args_list.push(".gitattributes".to_string());
    }

    // Also add .crab.toml if it exists.
    if root.join(".crab.toml").exists() {
        args_list.push(".crab.toml".to_string());
    }

    let output = Command::new("git")
        .args(&args_list)
        .current_dir(root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(stderr = %stderr, "git add returned non-zero exit code");
    }

    Ok(())
}

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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crab_types::pointer::{Pointer, is_pointer};
    use crab_xet::hash::MerkleHash;

    fn git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn head_only_adopt_streams_then_replaces_file_with_pointer() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        let path = dir.path().join("model.bin");
        let data: Vec<u8> = (0..(2 * 1024 * 1024) as u32)
            .map(|i| (i.wrapping_mul(1_103_515_245) >> 13) as u8)
            .collect();
        std::fs::write(&path, &data).unwrap();

        let args = AdoptArgs {
            patterns: vec!["*.bin".to_owned()],
            rewrite_history: false,
            force: false,
            dry_run: false,
            jobs: 1,
            mode: OutputMode::Json,
            interactive: false,
        };

        run_head_only(
            &[(path.clone(), data.len() as u64)],
            dir.path(),
            &args.patterns,
            &args,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let pointer_bytes = std::fs::read(&path).unwrap();
        assert!(is_pointer(&pointer_bytes));
        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.size, data.len() as u64);
        assert_eq!(pointer.file_hash, *blake3::hash(&data).as_bytes());

        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();
        let staged = staging
            .chunks_for_file(&MerkleHash::from(pointer.file_hash))
            .unwrap();
        assert!(!staged.is_empty());
    }
}
