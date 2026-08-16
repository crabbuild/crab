//! `crab why <file>` — explain a file's tracking and hydration state.
//!
//! Reports whether a file is crab-tracked (via `.gitattributes`
//! `filter=crab` rules), its current hydration state, and pointer
//! metadata when applicable.

use std::path::Path;

use serde::Serialize;

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::core::style::CliStyle;
use crate::engine::pointer::{self, HydrationState};
use crab_types::pointer::{Pointer, hex_encode};

/// JSON schema name for `crab why --json` output.
pub const WHY_SCHEMA: &str = "why";

/// Result of the `why` analysis for a single file.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct WhyResult {
    pub path: String,
    pub tracked: bool,
    pub tracking_rule: Option<String>,
    pub hydration_state: Option<String>,
    pub pointer_metadata: Option<PointerInfo>,
    pub on_disk_size: Option<u64>,
    pub size_matches_pointer: Option<bool>,
}

/// Pointer metadata extracted from a pointer blob.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PointerInfo {
    pub file_hash: String,
    pub original_size: u64,
    pub shard_hint: Option<String>,
}

#[derive(Debug)]
struct FileAnalysis {
    hydration_state: Option<&'static str>,
    pointer_metadata: Option<PointerInfo>,
    on_disk_size: Option<u64>,
    size_matches_pointer: Option<bool>,
}

/// Run the `why` command for a single file path.
pub fn run_why(file: &str, mode: OutputMode) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let file_path = cwd.join(file);

    if !file_path.exists() {
        return Err(CrabError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("file not found: {file}"),
        )));
    }

    let rel_path = file_path.strip_prefix(&cwd).unwrap_or(file_path.as_path());

    let (tracked, tracking_rule) = check_tracking(&cwd, rel_path);

    let analysis = if tracked {
        analyze_file(&file_path, &cwd, rel_path)?
    } else {
        FileAnalysis {
            hydration_state: None,
            pointer_metadata: None,
            on_disk_size: None,
            size_matches_pointer: None,
        }
    };

    let result = WhyResult {
        path: rel_path.to_string_lossy().into_owned(),
        tracked,
        tracking_rule,
        hydration_state: analysis.hydration_state.map(ToOwned::to_owned),
        pointer_metadata: analysis.pointer_metadata,
        on_disk_size: analysis.on_disk_size,
        size_matches_pointer: analysis.size_matches_pointer,
    };

    match mode {
        OutputMode::Json => {
            emit_json(WHY_SCHEMA, "1.0", &result);
        }
        _ => {
            render_text(&result, mode);
        }
    }

    Ok(())
}

/// Check whether a file is crab-tracked via `.gitattributes` rules.
///
/// Returns `(is_tracked, matching_rule)`.
fn check_tracking(root: &Path, rel_path: &Path) -> (bool, Option<String>) {
    let ga_path = root.join(".gitattributes");
    let Ok(content) = std::fs::read_to_string(&ga_path) else {
        return (false, None);
    };

    let rel_str = rel_path.to_string_lossy();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !trimmed.contains("filter=crab") {
            continue;
        }

        // Extract the glob pattern (first whitespace-delimited token).
        let Some(pattern) = trimmed.split_whitespace().next() else {
            continue;
        };

        if matches_pattern(pattern, &rel_str) {
            return (true, Some(trimmed.to_owned()));
        }
    }

    (false, None)
}

/// Simple glob matching for `.gitattributes` patterns.
///
/// Supports `*.ext` suffix matching, `**` / `*` catch-all, and exact
/// match. This mirrors the legacy matching used elsewhere in the
/// codebase.
fn matches_pattern(pattern: &str, path: &str) -> bool {
    if pattern == "*" || pattern == "**" || pattern == "**/*" {
        return true;
    }

    // `*.ext` suffix matching.
    if let Some(suffix) = pattern.strip_prefix('*')
        && path.ends_with(suffix)
    {
        return true;
    }

    // `dir/**` prefix matching.
    if let Some(prefix) = pattern.strip_suffix("/**")
        && (path.starts_with(prefix) || path.starts_with(&format!("{prefix}/")))
    {
        return true;
    }

    // Exact match.
    pattern == path
}

/// Analyze a tracked file's hydration state and pointer metadata.
fn analyze_file(abs_path: &Path, root: &Path, rel_path: &Path) -> Result<FileAnalysis> {
    let meta = std::fs::metadata(abs_path)?;
    let on_disk_size = meta.len();

    // Check if the file is currently a pointer on disk.
    if pointer::is_working_tree_pointer(abs_path)? {
        let contents = std::fs::read(abs_path)?;
        match Pointer::parse(&contents) {
            Ok(ptr) => {
                let info = PointerInfo {
                    file_hash: hex_encode(&ptr.file_hash),
                    original_size: ptr.size,
                    shard_hint: ptr.shard_hint.map(|h| hex_encode(&h)),
                };
                return Ok(FileAnalysis {
                    hydration_state: Some("pointer"),
                    pointer_metadata: Some(info),
                    on_disk_size: Some(on_disk_size),
                    size_matches_pointer: None,
                });
            }
            Err(_) => {
                return Ok(FileAnalysis {
                    hydration_state: Some("pointer"),
                    pointer_metadata: None,
                    on_disk_size: Some(on_disk_size),
                    size_matches_pointer: None,
                });
            }
        }
    }

    // File is not a pointer — check committed pointer to determine
    // hydrated vs modified.
    let committed_pointer = read_committed_pointer(root, rel_path);

    match committed_pointer {
        Some(ptr) => {
            let state = match pointer::detect_hydration_state(abs_path, &ptr)? {
                HydrationState::Pointer => "pointer",
                HydrationState::Hydrated => "hydrated",
                HydrationState::Modified => "modified",
            };
            let matches = on_disk_size == ptr.size;
            Ok(FileAnalysis {
                hydration_state: Some(state),
                pointer_metadata: None,
                on_disk_size: Some(on_disk_size),
                size_matches_pointer: Some(matches),
            })
        }
        None => {
            // No committed pointer — file is hydrated (or newly added).
            Ok(FileAnalysis {
                hydration_state: Some("hydrated"),
                pointer_metadata: None,
                on_disk_size: Some(on_disk_size),
                size_matches_pointer: None,
            })
        }
    }
}

/// Read the committed pointer blob for a tracked file from HEAD.
///
/// Returns `Some(Pointer)` when the committed blob is a valid crab
/// pointer, `None` otherwise.
fn read_committed_pointer(root: &Path, rel_path: &Path) -> Option<Pointer> {
    let rev_path = format!("HEAD:{}", rel_path.display());
    let output = std::process::Command::new("git")
        .args(["show", &rev_path])
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Pointer::parse(&output.stdout).ok()
}

/// Render the `WhyResult` as human-readable text output.
fn render_text(result: &WhyResult, mode: OutputMode) {
    let style = CliStyle::resolve(mode);

    println!("File: {}", result.path);
    println!();

    // Tracking status.
    if result.tracked {
        let rule = result.tracking_rule.as_deref().unwrap_or("filter=crab");
        println!("  {}", style.ok(&format!("Tracked by crab ({rule})")));
    } else {
        println!(
            "  {} Not tracked by crab",
            if style.is_enabled() { "○" } else { "-" }
        );
        println!();
        println!("  This file is not matched by any filter=crab rule in .gitattributes.");
        return;
    }

    // Hydration state.
    if let Some(state) = &result.hydration_state {
        let state_display = match state.as_str() {
            "pointer" => style.warn("Pointer (dehydrated)"),
            "hydrated" => style.ok("Hydrated"),
            "modified" => style.warn("Modified (size differs from committed pointer)"),
            other => format!("  {other}"),
        };
        println!("  State: {state_display}");
    }

    // Pointer metadata.
    if let Some(ref info) = result.pointer_metadata {
        println!();
        println!("  Pointer metadata:");
        println!("    file-hash:     {}", info.file_hash);
        println!("    original-size: {} bytes", info.original_size);
        if let Some(ref hint) = info.shard_hint {
            println!("    shard-hint:    {hint}");
        }
    }

    // On-disk size.
    if let Some(size) = result.on_disk_size {
        println!("  On-disk size: {size} bytes");
    }

    // Size match.
    if let Some(matches) = result.size_matches_pointer {
        if matches {
            println!("  {}", style.ok("Size matches committed pointer"));
        } else {
            println!("  {}", style.warn("Size does NOT match committed pointer"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crab_types::pointer::Pointer;

    fn sample_hash() -> [u8; 32] {
        let mut h = [0u8; 32];
        for (i, byte) in h.iter_mut().enumerate() {
            *byte = i as u8;
        }
        h
    }

    fn sample_pointer(size: u64) -> Pointer {
        Pointer {
            file_hash: sample_hash(),
            size,
            shard_hint: None,
        }
    }

    #[test]
    fn matches_pattern_star_ext() {
        assert!(matches_pattern("*.bin", "model.bin"));
        assert!(matches_pattern("*.bin", "sub/model.bin"));
        assert!(!matches_pattern("*.bin", "model.txt"));
    }

    #[test]
    fn matches_pattern_wildcard_all() {
        assert!(matches_pattern("*", "anything.txt"));
        assert!(matches_pattern("**", "deep/nested/file.rs"));
        assert!(matches_pattern("**/*", "deep/nested/file.rs"));
    }

    #[test]
    fn matches_pattern_exact() {
        assert!(matches_pattern("data.bin", "data.bin"));
        assert!(!matches_pattern("data.bin", "other.bin"));
    }

    #[test]
    fn matches_pattern_dir_prefix() {
        assert!(matches_pattern("models/**", "models/weights.bin"));
        assert!(matches_pattern("models/**", "models/sub/weights.bin"));
        assert!(!matches_pattern("models/**", "other/weights.bin"));
    }

    #[test]
    fn check_tracking_finds_matching_rule() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n*.safetensors filter=crab -text\n",
        )
        .unwrap();

        let (tracked, rule) = check_tracking(dir.path(), Path::new("model.bin"));
        assert!(tracked);
        assert_eq!(
            rule.as_deref(),
            Some("*.bin filter=crab diff=crab merge=crab -text")
        );
    }

    #[test]
    fn check_tracking_returns_false_for_untracked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        let (tracked, rule) = check_tracking(dir.path(), Path::new("readme.txt"));
        assert!(!tracked);
        assert!(rule.is_none());
    }

    #[test]
    fn check_tracking_returns_false_when_no_gitattributes() {
        let dir = tempfile::tempdir().unwrap();
        let (tracked, rule) = check_tracking(dir.path(), Path::new("model.bin"));
        assert!(!tracked);
        assert!(rule.is_none());
    }

    #[test]
    fn check_tracking_ignores_comments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "# *.bin filter=crab\n*.safetensors filter=crab -text\n",
        )
        .unwrap();

        let (tracked, _) = check_tracking(dir.path(), Path::new("model.bin"));
        assert!(!tracked);

        let (tracked, _) = check_tracking(dir.path(), Path::new("weights.safetensors"));
        assert!(tracked);
    }

    #[test]
    fn analyze_pointer_file() {
        let dir = tempfile::tempdir().unwrap();
        let ptr = sample_pointer(4096);
        let path = dir.path().join("model.bin");
        std::fs::write(&path, ptr.serialize()).unwrap();

        let analysis = analyze_file(&path, dir.path(), Path::new("model.bin")).unwrap();

        assert_eq!(analysis.hydration_state, Some("pointer"));
        assert!(analysis.pointer_metadata.is_some());
        let info = analysis.pointer_metadata.unwrap();
        assert_eq!(info.original_size, 4096);
        assert_eq!(info.file_hash.len(), 64);
        assert!(analysis.on_disk_size.is_some());
    }

    #[test]
    fn analyze_hydrated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, vec![0xAB; 8192]).unwrap();

        let analysis = analyze_file(&path, dir.path(), Path::new("data.bin")).unwrap();

        assert_eq!(analysis.hydration_state, Some("hydrated"));
        assert!(analysis.pointer_metadata.is_none());
        assert_eq!(analysis.on_disk_size, Some(8192));
    }

    #[test]
    fn run_why_file_not_found() {
        let err = run_why("/nonexistent/path/file.bin", OutputMode::Text);
        assert!(err.is_err());
    }

    #[test]
    fn why_result_json_serialization() {
        let result = WhyResult {
            path: "model.bin".to_owned(),
            tracked: true,
            tracking_rule: Some("*.bin filter=crab -text".to_owned()),
            hydration_state: Some("pointer".to_owned()),
            pointer_metadata: Some(PointerInfo {
                file_hash: "a".repeat(64),
                original_size: 1024,
                shard_hint: None,
            }),
            on_disk_size: Some(128),
            size_matches_pointer: None,
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"tracked\":true"));
        assert!(json.contains("\"hydration_state\":\"pointer\""));
        assert!(json.contains("\"original_size\":1024"));
    }
}
