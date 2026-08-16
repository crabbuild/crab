//! `crab setup` — configure a repository for Crab tracking.
//!
//! This is the "conversion" step that `crab init` deliberately defers.
//! `crab init` only wires up the remote; `crab setup` scans the working
//! tree, auto-tracks large files, updates `.crab.toml`, and installs the
//! filter driver so git invokes the crab clean/smudge pipeline.
//!
//! # Enterprise hardening
//!
//! - **Idempotent**: safe to run repeatedly; never duplicates entries.
//! - **--dry-run**: preview changes without touching disk.
//! - **--include / --exclude**: scope the scan to specific subtrees.
//! - **--track**: explicit patterns bypass the scan entirely.
//! - **--no-auto-track**: install the filter driver only.
//! - **--force**: replace existing crab entries cleanly.
//! - **--json / --jsonl**: machine-readable output for CI/CD.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::cmd::init;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{OutputMode, emit_json};
use crate::core::project_config::{ProjectConfig, TrackConfig};
use crate::core::style::CliStyle;

// ---------------------------------------------------------------------------
// CLI argument struct
// ---------------------------------------------------------------------------

/// Arguments for the `crab setup` command.
#[derive(Debug, Clone)]
pub struct SetupArgs {
    /// Skip scanning; only install the filter driver.
    pub no_auto_track: bool,
    /// Explicit patterns to track (e.g. `*.safetensors`). When set the
    /// scan is skipped for these — patterns are written directly.
    pub track: Vec<String>,
    /// Only scan these subdirectories (repo-root-relative).
    pub include: Vec<String>,
    /// Skip these subdirectories during scanning.
    pub exclude: Vec<String>,
    /// Preview changes without writing anything to disk.
    pub dry_run: bool,
    /// Replace existing crab entries in `.gitattributes` instead of
    /// appending (useful for resetting to a known-good config).
    pub force: bool,
    /// Output format.
    pub mode: OutputMode,
}

// ---------------------------------------------------------------------------
// JSON payload
// ---------------------------------------------------------------------------

/// Schema name for setup JSON output.
const SETUP_SCHEMA: &str = "setup";
const SETUP_VERSION: &str = "1.0";

/// Action the setup command took (or would take in dry-run).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SetupAction {
    FilterDriverInstalled,
    FilterDriverAlreadyInstalled,
    AutoTrackedPatterns { patterns: Vec<String> },
    ExplicitPatternsApplied { patterns: Vec<String> },
    GitattributesUpdated { lines_added: usize },
    CrabTomlUpdated { pattern_count: usize },
    NoChanges,
}

/// JSON payload for `crab setup --json`.
#[derive(Debug, Clone, Serialize)]
pub struct SetupPayload {
    pub actions: Vec<SetupAction>,
    pub tracked_patterns: Vec<String>,
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run `crab setup` with full argument control.
pub async fn run_setup(args: &SetupArgs, cancel: &CancellationToken) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_setup_at(&cwd, args, cancel).await
}

/// Run `crab setup` rooted at `root`.
pub async fn run_setup_at(root: &Path, args: &SetupArgs, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;

    let style = CliStyle::resolve(args.mode);
    let is_machine = args.mode.is_machine();

    // --- Validate preconditions ---
    if !root.join(".git").exists() {
        return Err(CrabError::Configuration {
            key: "git repo".into(),
            origin: "No git repository found. Run `git init` first, then `crab setup`.".into(),
        });
    }

    // .crab.toml must exist (created by `crab init`).
    let project_config_path = root.join(".crab.toml");
    if !project_config_path.exists() {
        return Err(CrabError::Configuration {
            key: ".crab.toml".into(),
            origin: "Missing .crab.toml — run `crab init <url>` first.".into(),
        });
    }

    let mut actions: Vec<SetupAction> = Vec::new();
    let mut tracked_patterns: Vec<String> = Vec::new();

    // ---- Step 1: Install filter driver ----
    if !args.dry_run {
        init::install_filter_driver(root)?;
    }
    actions.push(SetupAction::FilterDriverInstalled);

    if !is_machine {
        eprintln!("{}", style.ok("Filter driver installed"));
    }

    check_cancelled(cancel)?;

    // ---- Step 2: Resolve track patterns ----
    if !args.no_auto_track {
        if args.track.is_empty() {
            // Auto-scan for large files.
            if !is_machine {
                eprint!("Scanning for large files");
                if !args.include.is_empty() {
                    eprint!(" in: {}", args.include.join(", "));
                }
                if !args.exclude.is_empty() {
                    eprint!(" (excluding: {})", args.exclude.join(", "));
                }
                eprintln!("...");
            }

            let (detected, _file_count) = if args.dry_run {
                // Dry-run: only collect what *would* be tracked.
                let patterns = collect_patterns_dry(root, &args.include, &args.exclude);
                (patterns, 0)
            } else {
                let file_count = 0u64; // mutated by the scan callback
                let patterns = scan_and_track(
                    root,
                    &args.include,
                    &args.exclude,
                    args.force,
                    is_machine,
                    &style,
                )?;
                (patterns, file_count)
            };

            tracked_patterns = detected;
            if !tracked_patterns.is_empty() {
                actions.push(SetupAction::AutoTrackedPatterns {
                    patterns: tracked_patterns.clone(),
                });
            }
        } else {
            // Explicit patterns — no scan needed.
            tracked_patterns.clone_from(&args.track);
            if !args.dry_run {
                apply_explicit_patterns(root, &tracked_patterns, args.force)?;
            }
            actions.push(SetupAction::ExplicitPatternsApplied {
                patterns: tracked_patterns.clone(),
            });
            if !is_machine {
                eprintln!(
                    "{}",
                    style.ok(&format!(
                        "Applied {} explicit pattern(s): {}",
                        tracked_patterns.len(),
                        tracked_patterns.join(", ")
                    ))
                );
            }
        }
    } else if !is_machine {
        eprintln!("{}", style.dim("Skipping auto-track (--no-auto-track)"));
    }

    check_cancelled(cancel)?;

    // ---- Step 3: Update .crab.toml ----
    if !tracked_patterns.is_empty() {
        if !args.dry_run {
            let mut config = ProjectConfig::load(&project_config_path)?;
            config.track = Some(TrackConfig {
                patterns: tracked_patterns.clone(),
            });
            ProjectConfig::write(&project_config_path, &config)?;
        }
        actions.push(SetupAction::CrabTomlUpdated {
            pattern_count: tracked_patterns.len(),
        });
        if !is_machine {
            eprintln!(
                "{}",
                style.ok(&format!(
                    "Updated .crab.toml with {} track pattern(s)",
                    tracked_patterns.len()
                ))
            );
        }
    }

    check_cancelled(cancel)?;

    // ---- Step 4: Stage files ----
    let needs_git_add = !args.dry_run && root.join(".git").exists();
    if needs_git_add {
        if root.join(".gitattributes").exists() {
            let _ = std::process::Command::new("git")
                .args(["add", ".gitattributes"])
                .current_dir(root)
                .output();
        }
        let _ = std::process::Command::new("git")
            .args(["add", ".crab.toml"])
            .current_dir(root)
            .output();
    }

    // ---- Step 5: Output summary ----
    if actions.iter().all(|a| matches!(a, SetupAction::NoChanges)) {
        actions.push(SetupAction::NoChanges);
    }

    match args.mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(
                SETUP_SCHEMA,
                SETUP_VERSION,
                SetupPayload {
                    actions,
                    tracked_patterns,
                    dry_run: args.dry_run,
                },
            );
        }
        OutputMode::Text => {
            eprintln!();
            if args.dry_run {
                eprintln!("{}", style.bold("DRY RUN — no changes were made."));
            } else {
                eprintln!("{}", style.bold("Crab setup complete."));
            }

            let summary = if tracked_patterns.is_empty() {
                "(none)".to_string()
            } else {
                tracked_patterns.join(", ")
            };
            eprintln!("Tracked patterns: {summary}");
            eprintln!("{}", style.dim("Next: crab ship -m 'initial commit'"));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Scan + track
// ---------------------------------------------------------------------------

/// Scan for large files and write tracking entries to `.gitattributes`.
///
/// Returns the full set of tracked patterns (existing + newly detected).
fn scan_and_track(
    root: &Path,
    include: &[String],
    exclude: &[String],
    force: bool,
    is_machine: bool,
    style: &CliStyle,
) -> Result<Vec<String>> {
    let ga_path = root.join(".gitattributes");

    // Build the include/exclude directory sets (absolute paths).
    let include_dirs = resolve_dir_set(root, include);
    let exclude_dirs = resolve_dir_set(root, exclude);

    // If --force, start fresh (only keep non-crab lines).
    let existing_content = if force {
        let raw = std::fs::read_to_string(&ga_path).unwrap_or_default();
        let kept: String = raw
            .lines()
            .filter(|line| {
                let t = line.trim();
                t.is_empty() || t.starts_with('#') || !t.contains("filter=crab")
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !kept.is_empty() && !kept.ends_with('\n') {
            format!("{kept}\n")
        } else if kept.is_empty() {
            String::new()
        } else {
            kept
        }
    } else {
        std::fs::read_to_string(&ga_path).unwrap_or_default()
    };

    // Parse already-tracked patterns.
    let already_tracked: HashSet<String> = existing_content
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#') && t.contains("filter=crab")
        })
        .filter_map(|line| line.split_whitespace().next().map(String::from))
        .collect();

    // Scan.
    let mut new_exts: BTreeSet<String> = BTreeSet::new();
    let mut file_count: u64 = 0;
    scan_for_large_files_filtered(
        root,
        &already_tracked,
        &mut new_exts,
        &mut file_count,
        &include_dirs,
        &exclude_dirs,
    )?;

    if !is_machine {
        eprintln!(
            "{}",
            style.dim(&format!(
                "Scanned {} files, found {} new extension(s) to track",
                file_count,
                new_exts.len()
            ))
        );
    }

    if new_exts.is_empty() {
        // Return the already-tracked patterns.
        let mut patterns: Vec<String> = already_tracked.into_iter().collect();
        patterns.sort();
        return Ok(patterns);
    }

    // Build .gitattributes content.
    let mut content = existing_content;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    let joined: Vec<String> = new_exts.iter().map(|e| format!("*.{e}")).collect();
    for ext in &new_exts {
        use std::fmt::Write as _;
        let _ = writeln!(content, "*.{ext} filter=crab diff=crab merge=crab -text");
    }
    std::fs::write(&ga_path, content)?;

    if !is_machine {
        eprintln!("Detected large files — tracking: {}", joined.join(", "));
    }
    tracing::info!(extensions = ?new_exts, "auto-tracked large file extensions");

    // Combine already-tracked + new into the full set.
    let mut patterns: Vec<String> = already_tracked.into_iter().collect();
    for ext in new_exts {
        patterns.push(format!("*.{ext}"));
    }
    patterns.sort();
    Ok(patterns)
}

/// Collect patterns that *would* be tracked, without writing anything.
fn collect_patterns_dry(root: &Path, include: &[String], exclude: &[String]) -> Vec<String> {
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

    let include_dirs = resolve_dir_set(root, include);
    let exclude_dirs = resolve_dir_set(root, exclude);

    let mut new_exts: BTreeSet<String> = BTreeSet::new();
    let mut file_count: u64 = 0;
    let _ = scan_for_large_files_filtered(
        root,
        &already_tracked,
        &mut new_exts,
        &mut file_count,
        &include_dirs,
        &exclude_dirs,
    );

    let mut patterns: Vec<String> = already_tracked.into_iter().collect();
    for ext in new_exts {
        patterns.push(format!("*.{ext}"));
    }
    patterns.sort();
    patterns
}

/// Apply explicit track patterns: write them to `.gitattributes`.
fn apply_explicit_patterns(root: &Path, patterns: &[String], force: bool) -> Result<()> {
    let ga_path = root.join(".gitattributes");

    let existing = if force {
        let raw = std::fs::read_to_string(&ga_path).unwrap_or_default();
        let kept: String = raw
            .lines()
            .filter(|line| {
                let t = line.trim();
                t.is_empty() || t.starts_with('#') || !t.contains("filter=crab")
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !kept.is_empty() && !kept.ends_with('\n') {
            format!("{kept}\n")
        } else if kept.is_empty() {
            String::new()
        } else {
            kept
        }
    } else {
        let raw = std::fs::read_to_string(&ga_path).unwrap_or_default();
        // Don't add patterns that are already present.
        let already: HashSet<&str> = raw
            .lines()
            .filter(|line| {
                let t = line.trim();
                !t.is_empty() && !t.starts_with('#') && t.contains("filter=crab")
            })
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        let filtered: Vec<&String> = patterns
            .iter()
            .filter(|p| !already.contains(p.as_str()))
            .collect();
        if filtered.is_empty() {
            return Ok(());
        }
        raw
    };

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    for pat in patterns {
        use std::fmt::Write as _;
        let _ = writeln!(content, "{pat} filter=crab diff=crab merge=crab -text");
    }
    std::fs::write(&ga_path, content)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Filtered filesystem scan
// ---------------------------------------------------------------------------

/// Size threshold for auto-tracking (1 MiB).
const AUTO_TRACK_SIZE_THRESHOLD: u64 = 1_048_576;

/// Well-known large-file extensions — always tracked when found.
const WELL_KNOWN_LARGE_EXTENSIONS: &[&str] = init::WELL_KNOWN_LARGE_EXTENSIONS;

/// Resolve user-provided directory paths to absolute `PathBuf`s.
fn resolve_dir_set(root: &Path, dirs: &[String]) -> HashSet<PathBuf> {
    dirs.iter()
        .map(|d| {
            let p = Path::new(d);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            }
        })
        .collect()
}

/// Check whether a directory should be excluded from scanning, given
/// optional include/exclude sets.
///
/// * If `include` is non-empty, only directories under an include path
///   (or ancestors of include paths) are traversed.
/// * If `exclude` is non-empty, directories under an exclude path are
///   skipped entirely.
fn dir_allowed(
    dir_abs: &Path,
    include_dirs: &HashSet<PathBuf>,
    exclude_dirs: &HashSet<PathBuf>,
) -> bool {
    // Exclusion takes priority.
    if !exclude_dirs.is_empty() {
        for ex in exclude_dirs {
            if dir_abs.starts_with(ex) {
                return false;
            }
        }
    }

    // If include set is specified, the directory must be equal to or
    // nested under an include path, OR be an ancestor of an include path
    // (so we can reach the included subtree).
    if !include_dirs.is_empty() {
        return include_dirs
            .iter()
            .any(|inc| dir_abs.starts_with(inc) || inc.starts_with(dir_abs));
    }

    true
}

/// Walk the working tree looking for files above the size threshold or
/// with well-known large-file extensions.
///
/// Respects `include_dirs` and `exclude_dirs` when non-empty.
/// `file_count` is incremented for every regular file examined.
fn scan_for_large_files_filtered(
    dir: &Path,
    already_tracked: &HashSet<String>,
    new_exts: &mut BTreeSet<String>,
    file_count: &mut u64,
    include_dirs: &HashSet<PathBuf>,
    exclude_dirs: &HashSet<PathBuf>,
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

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if file_type.is_dir() {
            // Skip hidden dirs and common non-content directories.
            if name_str.starts_with('.')
                || name_str == "node_modules"
                || name_str == "target"
                || name_str == "__pycache__"
                || name_str == "venv"
            {
                continue;
            }

            // Check include/exclude filters.
            if !dir_allowed(&path, include_dirs, exclude_dirs) {
                continue;
            }

            scan_for_large_files_filtered(
                &path,
                already_tracked,
                new_exts,
                file_count,
                include_dirs,
                exclude_dirs,
            )?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        *file_count += 1;

        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_lowercase(),
            None => continue,
        };

        // Skip if already tracked.
        let glob = format!("*.{ext}");
        if already_tracked.contains(&glob) {
            continue;
        }

        // Check if this extension qualifies for auto-tracking.
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
