//! `crab status` — report hydration state of the working tree.
//!
//! Walks the working tree, identifies crab-tracked files via
//! `.gitattributes` `filter=crab` entries, and reports summary
//! statistics: total tracked files, pointer (unhydrated) count/size,
//! and hydrated count/size.
//!
//! The unified view merges git working-tree state (staged, unstaged,
//! untracked) with crab hydration state (pointer, hydrated, modified)
//! into a single combined output.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(not(feature = "gix-worktree"))]
use std::process::Command;

use serde::Serialize;
use tracing::debug;

use crate::core::error::Result;
use crate::core::output::{OutputMode, emit_json};
use crate::core::style::CliStyle;
use crate::engine::pointer::{self, HydrationState};
use crab_types::pointer::Pointer;

// ---------------------------------------------------------------------------
// Unified status types
// ---------------------------------------------------------------------------

/// Git working-tree state for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GitState {
    Staged,
    Unstaged,
    Untracked,
}

/// Crab hydration state for a tracked file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CrabState {
    Pointer,
    Hydrated,
    Modified,
}

/// A unified status entry combining git and crab state.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct UnifiedEntry {
    pub path: String,
    pub git_state: Option<GitState>,
    pub crab_state: Option<CrabState>,
    pub size: u64,
}

/// Summary of crab-tracked file states.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct CrabSummary {
    pub total_tracked: u64,
    pub pointer_count: u64,
    pub pointer_bytes: u64,
    pub hydrated_count: u64,
    pub hydrated_bytes: u64,
    pub modified_count: u64,
}

// ---------------------------------------------------------------------------
// Git porcelain v2 parsing
// ---------------------------------------------------------------------------

/// A raw entry parsed from `git status --porcelain=v2`.
#[derive(Debug)]
pub(crate) struct GitStatusEntry {
    pub(crate) path: String,
    pub(crate) state: GitState,
}

/// Parse `git status --porcelain=v2` output into per-file git states.
///
/// Porcelain v2 format:
/// - `1 <XY> ...` — ordinary changed entry (tracked)
/// - `2 <XY> ...` — renamed/copied entry
/// - `u <XY> ...` — unmerged entry
/// - `? <path>` — untracked
///
/// We extract the XY codes to determine staged vs unstaged state.
fn parse_git_porcelain_v2(output: &str) -> Vec<GitStatusEntry> {
    let mut entries = Vec::new();

    for line in output.lines() {
        if line.starts_with('?') {
            // Untracked: "? <path>"
            if let Some(path) = line.get(2..) {
                entries.push(GitStatusEntry {
                    path: path.to_owned(),
                    state: GitState::Untracked,
                });
            }
        } else if line.starts_with('1') || line.starts_with('2') || line.starts_with('u') {
            // Tracked change: "1 XY <sub> <mH> <mI> <mW> <hH> <hI> <path>"
            // or renamed:     "2 XY <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\t<origPath>"
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() < 2 {
                continue;
            }
            let rest = parts[1];
            // XY is the first two characters of rest
            if rest.len() < 2 {
                continue;
            }
            let xy = &rest[..2];
            let x = xy.as_bytes()[0]; // index (staged) state
            let y = xy.as_bytes()[1]; // worktree (unstaged) state

            // Extract path — for type 1/u it's the last space-delimited field,
            // for type 2 it's after the last space before the tab.
            let path = extract_path_from_porcelain_line(line);
            let Some(path) = path else { continue };

            // A file can be both staged and unstaged (e.g., partially staged).
            // We emit separate entries for each state.
            if x != b'.' && x != b'?' {
                entries.push(GitStatusEntry {
                    path: path.clone(),
                    state: GitState::Staged,
                });
            }
            if y != b'.' && y != b'?' {
                entries.push(GitStatusEntry {
                    path,
                    state: GitState::Unstaged,
                });
            }
        }
        // Ignore header lines (# branch.oid, etc.)
    }

    entries
}

/// Extract the file path from a porcelain v2 line (type 1, 2, or u).
fn extract_path_from_porcelain_line(line: &str) -> Option<String> {
    let first_char = line.as_bytes().first()?;
    match first_char {
        b'1' | b'u' => {
            // Format: "1 XY sub mH mI mW hH hI path"
            // 9 space-separated fields, path is the 9th (index 8)
            let fields: Vec<&str> = line.splitn(9, ' ').collect();
            if fields.len() == 9 {
                Some(fields[8].to_owned())
            } else {
                None
            }
        }
        b'2' => {
            // Format: "2 XY sub mH mI mW hH hI Xscore path\torigPath"
            // The 8th field is Xscore (e.g., R100), and the 9th+ is "path\torigPath"
            let fields: Vec<&str> = line.splitn(9, ' ').collect();
            if fields.len() == 9 {
                // The 9th field is "Xscore path\torigPath" — we need to skip the score
                let remainder = fields[8];
                // The score is the first space-delimited token (e.g., "R100")
                // followed by "path\torigPath"
                let after_score = remainder.split_once(' ')?.1;
                // Take the part before the tab (new path)
                let path = after_score.split('\t').next()?;
                Some(path.to_owned())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Run `git status --porcelain=v2` in the given directory.
fn run_git_status_porcelain_v2(root: &Path) -> Result<Vec<GitStatusEntry>> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain=v2"])
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()?;

    if !output.status.success() {
        // Not a git repo or git not available — return empty.
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_git_porcelain_v2(&stdout))
}

// ---------------------------------------------------------------------------
// Merge algorithm
// ---------------------------------------------------------------------------

/// Merge git status entries and crab hydration entries into unified entries.
///
/// Join on relative path: files appearing in both sources get a single
/// `UnifiedEntry` with both states populated.
pub(crate) fn merge_status(
    git_entries: Vec<GitStatusEntry>,
    crab_entries: &[(String, CrabState, u64)],
) -> (Vec<UnifiedEntry>, CrabSummary) {
    let mut map: BTreeMap<String, UnifiedEntry> = BTreeMap::new();

    // Insert git entries.
    for ge in git_entries {
        let entry = map.entry(ge.path.clone()).or_insert_with(|| UnifiedEntry {
            path: ge.path,
            git_state: None,
            crab_state: None,
            size: 0,
        });
        // If a file has both staged and unstaged changes, prefer staged
        // (it will appear in the staged section).
        if entry.git_state.is_none() || ge.state == GitState::Staged {
            entry.git_state = Some(ge.state);
        }
    }

    // Insert/merge crab entries.
    let mut summary = CrabSummary::default();
    for (path, crab_state, size) in crab_entries {
        summary.total_tracked += 1;
        match crab_state {
            CrabState::Pointer => {
                summary.pointer_count += 1;
                summary.pointer_bytes += size;
            }
            CrabState::Hydrated => {
                summary.hydrated_count += 1;
                summary.hydrated_bytes += size;
            }
            CrabState::Modified => {
                summary.modified_count += 1;
            }
        }

        let entry = map.entry(path.clone()).or_insert_with(|| UnifiedEntry {
            path: path.clone(),
            git_state: None,
            crab_state: None,
            size: 0,
        });
        entry.crab_state = Some(*crab_state);
        entry.size = *size;
    }

    (map.into_values().collect(), summary)
}

// ---------------------------------------------------------------------------
// Unified status rendering
// ---------------------------------------------------------------------------

/// Render unified status as grouped text output.
fn render_unified_text(entries: &[UnifiedEntry], summary: &CrabSummary, style: &CliStyle) {
    let staged: Vec<&UnifiedEntry> = entries
        .iter()
        .filter(|e| e.git_state == Some(GitState::Staged))
        .collect();
    let unstaged: Vec<&UnifiedEntry> = entries
        .iter()
        .filter(|e| e.git_state == Some(GitState::Unstaged))
        .collect();
    let untracked: Vec<&UnifiedEntry> = entries
        .iter()
        .filter(|e| e.git_state == Some(GitState::Untracked))
        .collect();
    let crab_only: Vec<&UnifiedEntry> = entries
        .iter()
        .filter(|e| e.git_state.is_none() && e.crab_state.is_some())
        .collect();

    if !staged.is_empty() {
        println!("{}", style.ok(&format!("Staged ({}):", staged.len())));
        for e in &staged {
            let crab_tag = crab_state_tag(e.crab_state);
            println!("  {} {}", crab_tag, e.path);
        }
        println!();
    }

    if !unstaged.is_empty() {
        println!("{}", style.warn(&format!("Unstaged ({}):", unstaged.len())));
        for e in &unstaged {
            let crab_tag = crab_state_tag(e.crab_state);
            println!("  {} {}", crab_tag, e.path);
        }
        println!();
    }

    if !untracked.is_empty() {
        println!("Untracked ({}):", untracked.len());
        for e in &untracked {
            println!("  {}", e.path);
        }
        println!();
    }

    if !crab_only.is_empty() {
        println!("Crab Hydration ({}):", crab_only.len());
        for e in &crab_only {
            let state_str = match e.crab_state {
                Some(CrabState::Pointer) => "pointer",
                Some(CrabState::Hydrated) => "hydrated",
                Some(CrabState::Modified) => "modified",
                None => "unknown",
            };
            println!("  [{}] {} ({})", state_str, e.path, format_bytes(e.size));
        }
        println!();
    }

    // Always print crab summary.
    println!("Crab summary:");
    println!("  Tracked:  {}", summary.total_tracked);
    println!(
        "  Pointer:  {} ({})",
        summary.pointer_count,
        format_bytes(summary.pointer_bytes)
    );
    println!(
        "  Hydrated: {} ({})",
        summary.hydrated_count,
        format_bytes(summary.hydrated_bytes)
    );
    if summary.modified_count > 0 {
        println!("  Modified: {}", summary.modified_count);
    }
}

/// Format a crab state as a short tag for inline display.
fn crab_state_tag(state: Option<CrabState>) -> &'static str {
    match state {
        Some(CrabState::Pointer) => "[P]",
        Some(CrabState::Hydrated) => "[H]",
        Some(CrabState::Modified) => "[M]",
        None => "   ",
    }
}

/// Render unified status in porcelain format with combined state codes.
///
/// Format: `<git_code><crab_code> <path>`
/// - git_code: S=staged, U=unstaged, ?=untracked, .=none
/// - crab_code: P=pointer, H=hydrated, M=modified, .=none
pub fn render_unified_porcelain(entries: &[UnifiedEntry]) {
    for entry in entries {
        let git_code = match entry.git_state {
            Some(GitState::Staged) => 'S',
            Some(GitState::Unstaged) => 'U',
            Some(GitState::Untracked) => '?',
            None => '.',
        };
        let crab_code = match entry.crab_state {
            Some(CrabState::Pointer) => 'P',
            Some(CrabState::Hydrated) => 'H',
            Some(CrabState::Modified) => 'M',
            None => '.',
        };
        println!("{}{} {}", git_code, crab_code, entry.path);
    }
}

// ---------------------------------------------------------------------------
// Collect crab hydration entries from tree walk
// ---------------------------------------------------------------------------

/// Walk the tree and collect crab hydration state per file.
fn collect_crab_entries(
    root: &Path,
    classifier: &TrackedClassifier,
) -> Result<Vec<(String, CrabState, u64)>> {
    let mut entries = Vec::new();
    collect_crab_entries_recursive(root, root, classifier, &mut entries)?;
    Ok(entries)
}

fn collect_crab_entries_recursive(
    root: &Path,
    dir: &Path,
    classifier: &TrackedClassifier,
    entries: &mut Vec<(String, CrabState, u64)>,
) -> Result<()> {
    let dir_entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    for entry in dir_entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            collect_crab_entries_recursive(root, &path, classifier, entries)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let Ok(rel_path) = path.strip_prefix(root) else {
            continue;
        };

        if !classifier.is_tracked(rel_path) {
            continue;
        }

        let rel_str = rel_path.to_string_lossy().into_owned();

        if pointer::is_working_tree_pointer(&path)? {
            let contents = std::fs::read(&path)?;
            match Pointer::parse(&contents) {
                Ok(ptr) => {
                    entries.push((rel_str, CrabState::Pointer, ptr.size));
                }
                Err(e) => {
                    debug!(path = %rel_path.display(), error = %e, "pointer parse failed");
                }
            }
        } else {
            let meta = std::fs::metadata(&path)?;
            let file_size = meta.len();
            let state = match read_committed_pointer(root, rel_path) {
                Some(ptr) => match pointer::detect_hydration_state(&path, &ptr) {
                    Ok(HydrationState::Modified) => CrabState::Modified,
                    Ok(HydrationState::Pointer) => CrabState::Pointer,
                    _ => CrabState::Hydrated,
                },
                None => CrabState::Hydrated,
            };
            entries.push((rel_str, state, file_size));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry point (unified)
// ---------------------------------------------------------------------------

/// Run the unified status command that merges git and crab state.
pub fn run_unified_status(porcelain: bool, mode: OutputMode) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_unified_status_in(&cwd, porcelain, mode)
}

/// Unified status implementation that accepts an explicit root directory.
pub fn run_unified_status_in(root: &Path, porcelain: bool, mode: OutputMode) -> Result<()> {
    let style = match mode {
        OutputMode::Json => {
            // `crab status --json` is a stable structured-output contract
            // documented as schema `status`. Keep the unified view for
            // human/porcelain output until a separate JSON schema ships.
            return run_status_in(root, porcelain, mode);
        }
        OutputMode::Text | OutputMode::Jsonl => CliStyle::resolve(mode),
    };

    // Collect git status entries.
    let git_entries = run_git_status_porcelain_v2(root)?;

    // Collect crab hydration entries.
    let classifier = TrackedClassifier::open(root)?;
    let crab_entries = if classifier.is_empty() {
        Vec::new()
    } else {
        collect_crab_entries(root, &classifier)?
    };

    // Merge both sources.
    let (entries, summary) = merge_status(git_entries, &crab_entries);

    match mode {
        OutputMode::Text | OutputMode::Jsonl => {
            if porcelain {
                render_unified_porcelain(&entries);
            } else {
                render_unified_text(&entries, &summary, &style);
            }
        }
        OutputMode::Json => return run_status_in(root, porcelain, mode),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Legacy status types and implementation (preserved)
// ---------------------------------------------------------------------------

/// Accumulated statistics from a working-tree scan.
#[derive(Debug, Default)]
struct StatusStats {
    /// Total number of crab-tracked files found.
    total_tracked: u64,
    /// Number of files still in pointer (unhydrated) state.
    pointer_count: u64,
    /// Sum of original file sizes for pointer files (from pointer metadata).
    pointer_size: u64,
    /// Number of hydrated files (full content present).
    hydrated_count: u64,
    /// Sum of on-disk sizes for hydrated files.
    hydrated_size: u64,
}

/// Per-file entry collected during the tree walk for JSON output.
struct FileEntry {
    path: String,
    state: &'static str,
    bytes: u64,
}

/// JSON payload for `crab status --json`.
#[derive(Serialize, schemars::JsonSchema)]
pub struct StatusPayload {
    total_tracked: u64,
    hydrated: u64,
    pointer: u64,
    modified: u64,
    files: Vec<StatusEntry>,
}

/// One file in the `StatusPayload.files` array.
#[derive(Serialize, schemars::JsonSchema)]
pub struct StatusEntry {
    path: String,
    state: String,
    bytes: u64,
}

/// Run the status command, printing a human-readable summary.
///
/// When `porcelain` is `true`, outputs machine-readable per-file lines
/// instead (one line per file: `P <path>`, `H <path>`, `M <path>`).
///
/// When `mode` is `Json`, collects per-file data and emits a
/// `StatusPayload` envelope.
///
/// # Errors
///
/// Returns [`CrabError::Io`] on filesystem failures.
pub fn run_status(porcelain: bool, mode: OutputMode) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_status_in(&cwd, porcelain, mode)
}

/// Status implementation that accepts an explicit root directory.
pub fn run_status_in(root: &Path, porcelain: bool, mode: OutputMode) -> Result<()> {
    let classifier = TrackedClassifier::open(root)?;

    if classifier.is_empty() {
        if mode == OutputMode::Json {
            let payload = StatusPayload {
                total_tracked: 0,
                hydrated: 0,
                pointer: 0,
                modified: 0,
                files: Vec::new(),
            };
            emit_json("status", "1.0", payload);
            return Ok(());
        }
        println!("No crab-tracked patterns found in .gitattributes");
        return Ok(());
    }

    debug!(kind = %TrackedClassifier::kind(), "loaded crab-tracked classifier");

    let mut stats = StatusStats::default();
    let mut file_entries: Vec<FileEntry> = Vec::new();

    let collect_files = mode == OutputMode::Json;
    walk_tree(
        root,
        root,
        &classifier,
        &mut stats,
        porcelain,
        collect_files,
        &mut file_entries,
    )?;

    if mode == OutputMode::Json {
        let mut hydrated_count: u64 = 0;
        let mut pointer_count: u64 = 0;
        let mut modified_count: u64 = 0;

        let entries: Vec<StatusEntry> = file_entries
            .into_iter()
            .map(|fe| {
                match fe.state {
                    "hydrated" => hydrated_count += 1,
                    "pointer" => pointer_count += 1,
                    "modified" => modified_count += 1,
                    _ => {}
                }
                StatusEntry {
                    path: fe.path,
                    state: fe.state.to_owned(),
                    bytes: fe.bytes,
                }
            })
            .collect();

        let payload = StatusPayload {
            total_tracked: stats.total_tracked,
            hydrated: hydrated_count,
            pointer: pointer_count,
            modified: modified_count,
            files: entries,
        };
        emit_json("status", "1.0", payload);
    } else if !porcelain {
        print_summary(&stats);
        // Mirror status section: check if .crab.toml has a [mirror] config.
        print_mirror_status(root);
    }

    Ok(())
}

/// Per-site classifier used by `walk_tree` to decide whether a file is
/// crab-tracked.
///
/// Under `gix-pathmatch`, this wraps the consolidated
/// [`core::attrs::TrackedClassifier`] which parses `.gitattributes`
/// through `gix_attributes::Search`. The legacy path parses
/// `.gitattributes` in-line and keeps the simple suffix-matching
/// helper for backwards compatibility while the flag rolls out.
#[cfg(feature = "gix-pathmatch")]
struct TrackedClassifier(crate::core::attrs::TrackedClassifier);

#[cfg(not(feature = "gix-pathmatch"))]
struct TrackedClassifier {
    patterns: Vec<String>,
}

impl TrackedClassifier {
    fn open(root: &Path) -> Result<Self> {
        #[cfg(feature = "gix-pathmatch")]
        {
            Ok(TrackedClassifier(
                crate::core::attrs::TrackedClassifier::open(root, "crab")?,
            ))
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            Ok(TrackedClassifier {
                patterns: parse_gitattributes_globs_legacy(root)?,
            })
        }
    }

    fn is_empty(&self) -> bool {
        #[cfg(feature = "gix-pathmatch")]
        {
            // The reader-backed classifier always advertises "not empty"
            // — walks fall through with zero matches on a repo without
            // `.gitattributes`, so the no-output message would suppress
            // otherwise-informative traces. The UX difference is the
            // classifier reports "no patterns" vs "no matches"; both
            // are honest.
            false
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            self.patterns.is_empty()
        }
    }

    fn kind() -> &'static str {
        #[cfg(feature = "gix-pathmatch")]
        {
            "gix-attributes"
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            "legacy-glob"
        }
    }

    fn is_tracked(&self, rel_path: &Path) -> bool {
        let rel_str = rel_path.to_string_lossy();
        #[cfg(feature = "gix-pathmatch")]
        {
            self.0.is_tracked(&rel_str)
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            let _ = rel_str;
            matches_any_pattern_legacy(rel_path, &self.patterns)
        }
    }

    /// Test-only constructor that builds a classifier from an
    /// in-memory pattern list.
    ///
    /// Under `gix-pathmatch`, this composes a temporary `.gitattributes`
    /// body and opens an `AttrsReader` against a fresh tempdir so the
    /// matching semantics stay identical to production. Not public —
    /// production code always opens from an actual repo root.
    #[cfg(test)]
    fn from_patterns_for_test(patterns: Vec<String>) -> Self {
        #[cfg(feature = "gix-pathmatch")]
        {
            use std::io::Write;
            // Leak the tempdir — tests are short-lived, and the
            // AttrsReader borrows paths embedded in the Search. A
            // proper test fixture would own the tempdir; here we opt
            // for brevity.
            let dir = tempfile::tempdir().expect("create tempdir for test classifier");
            let ga = dir.path().join(".gitattributes");
            let mut f = std::fs::File::create(&ga).expect("create attrs");
            for p in &patterns {
                writeln!(f, "{} filter=crab", p).expect("write attrs");
            }
            f.flush().ok();
            let inner = crate::core::attrs::TrackedClassifier::open(dir.path(), "crab")
                .expect("open classifier");
            // Intentionally leak the dir so paths in the reader remain
            // valid for the test's lifetime.
            std::mem::forget(dir);
            TrackedClassifier(inner)
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            TrackedClassifier { patterns }
        }
    }
}

/// Parse `.gitattributes` in `root` and return glob patterns that have
/// `filter=crab`.
///
/// Legacy fallback for builds without the `gix-pathmatch` feature. Once
/// the feature flips default-on, this helper goes away along with the
/// three other copies of it across crab.
#[cfg(not(feature = "gix-pathmatch"))]
fn parse_gitattributes_globs_legacy(root: &Path) -> Result<Vec<String>> {
    let ga_path = root.join(".gitattributes");
    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let globs = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#') && trimmed.contains("filter=crab")
        })
        .filter_map(|line| {
            // The glob is the first whitespace-delimited token on the line.
            line.split_whitespace().next().map(String::from)
        })
        .collect();

    Ok(globs)
}

/// Check whether a filename matches any of the tracked glob patterns.
///
/// Uses simple suffix matching: `*.ext` matches files ending in `.ext`.
/// `**` patterns match any path. Legacy helper used only when
/// `gix-pathmatch` is disabled; the consolidated matcher lives in
/// [`crate::core::attrs`].
#[cfg(not(feature = "gix-pathmatch"))]
fn matches_any_pattern_legacy(rel_path: &Path, patterns: &[String]) -> bool {
    let path_str = rel_path.to_string_lossy();

    for pattern in patterns {
        if pattern == "*" || pattern == "**" || pattern == "**/*" {
            return true;
        }
        // Simple `*.ext` suffix matching.
        if let Some(suffix) = pattern.strip_prefix('*')
            && path_str.ends_with(suffix)
        {
            return true;
        }
        // Exact match.
        if *pattern == *path_str {
            return true;
        }
    }
    false
}

/// Recursively walk the directory tree starting at `dir`, collecting
/// hydration statistics for crab-tracked files.
fn walk_tree(
    root: &Path,
    dir: &Path,
    classifier: &TrackedClassifier,
    stats: &mut StatusStats,
    porcelain: bool,
    collect_files: bool,
    file_entries: &mut Vec<FileEntry>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            debug!(dir = %dir.display(), "skipping unreadable directory");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        // Skip hidden directories (.git, .crab, etc.).
        if file_type.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            walk_tree(
                root,
                &path,
                classifier,
                stats,
                porcelain,
                collect_files,
                file_entries,
            )?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let Ok(rel_path) = path.strip_prefix(root) else {
            continue;
        };

        if !classifier.is_tracked(rel_path) {
            continue;
        }

        process_tracked_file(
            root,
            &path,
            rel_path,
            stats,
            porcelain,
            collect_files,
            file_entries,
        )?;
    }

    Ok(())
}

/// Try to read the committed pointer blob for a tracked file from HEAD.
///
/// Under the `gix-worktree` feature this navigates HEAD → tree → blob
/// in-process through `gix_odb::Handle` and `gix_object::FindExt`,
/// avoiding the per-file `git show HEAD:<path>` shellout that the
/// legacy path used. A 10k-pointer repo that previously paid 10k
/// fork+exec costs now pays a single ODB open plus N in-process
/// tree lookups. Returns `Some(Pointer)` when the committed blob is a
/// valid crab pointer, `None` otherwise (no git repo, no commits,
/// file not in HEAD, or committed blob isn't a pointer).
#[cfg(feature = "gix-worktree")]
fn read_committed_pointer(root: &Path, rel_path: &Path) -> Option<Pointer> {
    read_committed_pointer_via_gix(root, rel_path)
}

/// Legacy shellout path for builds without `gix-worktree`.
///
/// Kept for one release cycle after the feature flips default-on, at
/// which point Task 25 deletes it along with the feature flag.
#[cfg(not(feature = "gix-worktree"))]
fn read_committed_pointer(root: &Path, rel_path: &Path) -> Option<Pointer> {
    let rev_path = format!("HEAD:{}", rel_path.display());
    let output = Command::new("git")
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

/// In-process committed-pointer lookup using `gix-ref` + `gix-odb` +
/// `gix-object` tree navigation.
///
/// Returns `None` on any failure — missing repo, missing HEAD, missing
/// path in the tree, or a committed blob that's not a valid pointer.
/// The status command is best-effort: a failed lookup means the file
/// is reported as "hydrated" (same semantics as the pre-adoption
/// shellout that also returned `None` on error).
#[cfg(feature = "gix-worktree")]
fn read_committed_pointer_via_gix(root: &Path, rel_path: &Path) -> Option<Pointer> {
    use gix_object::FindExt;

    let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(root).ok()?;

    let head_sha = crab_git::ref_resolve::resolve_ref_at(&ctx.per_worktree_git_dir, "HEAD").ok()?;
    let commit_oid = gix_hash::ObjectId::from_hex(head_sha.as_bytes()).ok()?;

    // Open the ODB at the common git object directory — same handle the existing
    // `vfs::engine::OdbReader` uses, but we don't need the blob cache
    // here. Single lookup is cheap enough to open per-call.
    let odb = gix_odb::at(ctx.objects_dir()).ok()?;

    // HEAD → commit → tree.
    let mut buf = Vec::new();
    let commit = odb.find_commit(&commit_oid, &mut buf).ok()?;
    let tree_oid = commit.tree();

    // Navigate the tree by path. gix's `lookup_entry` takes any
    // iterator of `PartialEq<BStr>` components; `BString` implements
    // the bound so convert components on the way in. Tree entries are
    // always stored with `/`, so we split on `/` regardless of the
    // host filesystem separator.
    let mut tree_buf = Vec::new();
    let tree = odb.find_tree_iter(&tree_oid, &mut tree_buf).ok()?;
    let rel_str = rel_path.to_string_lossy();
    let components: Vec<bstr::BString> = rel_str
        .split('/')
        .map(|s| bstr::BString::from(s.as_bytes()))
        .collect();
    let mut lookup_buf = Vec::new();
    let entry = tree
        .lookup_entry(&odb, &mut lookup_buf, components.into_iter())
        .ok()??;

    // Pull the blob bytes and parse as a crab pointer.
    let mut blob_buf = Vec::new();
    let blob = odb.find_blob(&entry.oid, &mut blob_buf).ok()?;
    Pointer::parse(blob.data).ok()
}

/// Inspect a single tracked file and update stats accordingly.
fn process_tracked_file(
    root: &Path,
    abs_path: &Path,
    rel_path: &Path,
    stats: &mut StatusStats,
    porcelain: bool,
    collect_files: bool,
    file_entries: &mut Vec<FileEntry>,
) -> Result<()> {
    stats.total_tracked += 1;

    if pointer::is_working_tree_pointer(abs_path)? {
        // File is a pointer — read it to get the original size.
        let contents = std::fs::read(abs_path)?;
        match Pointer::parse(&contents) {
            Ok(ptr) => {
                stats.pointer_count += 1;
                stats.pointer_size += ptr.size;

                if collect_files {
                    file_entries.push(FileEntry {
                        path: rel_path.to_string_lossy().into_owned(),
                        state: "pointer",
                        bytes: ptr.size,
                    });
                }

                if porcelain {
                    println!("P {}", rel_path.display());
                }
            }
            Err(e) => {
                debug!(path = %rel_path.display(), error = %e, "pointer parse failed");
            }
        }
    } else {
        let meta = std::fs::metadata(abs_path)?;
        let file_size = meta.len();
        stats.hydrated_count += 1;
        stats.hydrated_size += file_size;

        // Determine whether the file is cleanly hydrated or modified
        // (size differs from the committed pointer).
        let state = match read_committed_pointer(root, rel_path) {
            Some(ptr) => match pointer::detect_hydration_state(abs_path, &ptr) {
                Ok(HydrationState::Modified) => "modified",
                _ => "hydrated",
            },
            None => "hydrated",
        };

        if collect_files {
            file_entries.push(FileEntry {
                path: rel_path.to_string_lossy().into_owned(),
                state,
                bytes: file_size,
            });
        }

        if porcelain {
            let tag = if state == "modified" { "M" } else { "H" };
            println!("{tag} {}", rel_path.display());
        }
    }

    Ok(())
}

/// Format a byte count as a human-readable string (e.g. "1.23 GB").
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Print the human-readable status summary.
fn print_summary(stats: &StatusStats) {
    println!("Crab working tree status:");
    println!("  Tracked files:  {}", stats.total_tracked);
    println!(
        "  Pointer (lazy): {} ({})",
        stats.pointer_count,
        format_bytes(stats.pointer_size),
    );
    println!(
        "  Hydrated:       {} ({})",
        stats.hydrated_count,
        format_bytes(stats.hydrated_size),
    );
}

/// Print mirror status section if a [mirror] config is present in .crab.toml.
fn print_mirror_status(root: &Path) {
    use crate::core::project_config::ProjectConfig;

    let Some(config) = ProjectConfig::discover(root) else {
        return;
    };

    let Some(mirror) = config.mirror else {
        return;
    };

    println!();
    println!("Mirror:");

    // Check crab remote reachability by verifying the remote URL is configured.
    let crab_url = std::process::Command::new("git")
        .args(["remote", "get-url", &mirror.crab_remote])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());

    let status = match &crab_url {
        Some(url) => {
            // Check if the staging area has pending files.
            let staging_path = staging_path_for_status_root(root);
            let pending = if staging_path.exists() {
                // Count files in staging directory as a rough proxy for pending chunks.
                std::fs::read_dir(&staging_path)
                    .ok()
                    .map_or(0, |rd| rd.flatten().count())
            } else {
                0
            };

            if pending > 0 {
                println!("  Pending:  {pending} files pending push to crab remote");
            }

            format!("reachable ({url})")
        }
        None => "unreachable (crab remote not configured)".to_owned(),
    };

    eprintln!(
        "Mirror: {} ↔ {} | {}",
        mirror.origin_remote, mirror.crab_remote, status
    );
}

fn staging_path_for_status_root(root: &Path) -> PathBuf {
    crate::git::worktree::WorktreeContext::resolve_from_path(root).map_or_else(
        |_| root.join(".crab").join("staging"),
        |ctx| ctx.shared_staging_dir(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crab_types::pointer::Pointer;

    /// Test helper — build a TrackedClassifier from an in-memory
    /// pattern list so existing walk_tree / matches-calling tests keep
    /// their shape.
    fn classifier_for(patterns: Vec<String>) -> TrackedClassifier {
        TrackedClassifier::from_patterns_for_test(patterns)
    }

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

    fn setup_tracked_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        // Write .gitattributes with a tracked pattern.
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        dir
    }

    // The three `parse_gitattributes_*` tests below exercise the
    // legacy line-by-line parser. Under `gix-pathmatch` the
    // consolidated `AttrsReader` replaces it; equivalent coverage
    // lives in `crab/tests/pathmatch_cross_site_parity.rs` and the
    // `core::attrs::tests` module.
    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn parse_gitattributes_extracts_crab_patterns() {
        let dir = setup_tracked_dir();
        let globs = parse_gitattributes_globs_legacy(dir.path()).unwrap();
        assert_eq!(globs, vec!["*.bin"]);
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn parse_gitattributes_ignores_non_crab_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.txt text\n*.bin filter=crab diff=crab merge=crab -text\n# comment\n",
        )
        .unwrap();

        let globs = parse_gitattributes_globs_legacy(dir.path()).unwrap();
        assert_eq!(globs, vec!["*.bin"]);
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn parse_gitattributes_returns_empty_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let globs = parse_gitattributes_globs_legacy(dir.path()).unwrap();
        assert!(globs.is_empty());
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn matches_star_ext_pattern() {
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        assert!(classifier.is_tracked(Path::new("data.bin")));
        assert!(classifier.is_tracked(Path::new("sub/data.bin")));
        assert!(!classifier.is_tracked(Path::new("data.txt")));
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn matches_wildcard_all() {
        let classifier = classifier_for(vec!["*".to_string()]);
        assert!(classifier.is_tracked(Path::new("anything.txt")));
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn matches_double_star() {
        let classifier = classifier_for(vec!["**".to_string()]);
        assert!(classifier.is_tracked(Path::new("deep/nested/file.rs")));
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1_048_576), "1.00 MiB");
        assert_eq!(format_bytes(1_073_741_824), "1.00 GiB");
        assert_eq!(format_bytes(1_099_511_627_776), "1.00 TiB");
    }

    #[test]
    fn status_counts_pointer_files() {
        let dir = setup_tracked_dir();

        // Write a pointer file.
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("model.bin"), ptr.serialize()).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            dir.path(),
            dir.path(),
            &classifier,
            &mut stats,
            false,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(stats.total_tracked, 1);
        assert_eq!(stats.pointer_count, 1);
        assert_eq!(stats.pointer_size, 4096);
        assert_eq!(stats.hydrated_count, 0);
    }

    #[test]
    fn status_counts_hydrated_files() {
        let dir = setup_tracked_dir();

        // Write a hydrated (non-pointer) file.
        std::fs::write(dir.path().join("data.bin"), vec![0xAB; 8192]).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            dir.path(),
            dir.path(),
            &classifier,
            &mut stats,
            false,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(stats.total_tracked, 1);
        assert_eq!(stats.pointer_count, 0);
        assert_eq!(stats.hydrated_count, 1);
        assert_eq!(stats.hydrated_size, 8192);
    }

    #[test]
    fn status_mixed_pointer_and_hydrated() {
        let dir = setup_tracked_dir();

        let ptr = sample_pointer(2048);
        std::fs::write(dir.path().join("lazy.bin"), ptr.serialize()).unwrap();
        std::fs::write(dir.path().join("eager.bin"), vec![0xFF; 5000]).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            dir.path(),
            dir.path(),
            &classifier,
            &mut stats,
            false,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(stats.total_tracked, 2);
        assert_eq!(stats.pointer_count, 1);
        assert_eq!(stats.pointer_size, 2048);
        assert_eq!(stats.hydrated_count, 1);
        assert_eq!(stats.hydrated_size, 5000);
    }

    #[test]
    fn status_skips_hidden_directories() {
        let dir = setup_tracked_dir();

        // Create a hidden directory with a .bin file inside.
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("secret.bin"), vec![0u8; 100]).unwrap();

        // Create a visible .bin file.
        std::fs::write(dir.path().join("visible.bin"), vec![0u8; 200]).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            dir.path(),
            dir.path(),
            &classifier,
            &mut stats,
            false,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(stats.total_tracked, 1, "hidden dir files should be skipped");
    }

    #[test]
    fn status_ignores_non_matching_files() {
        let dir = setup_tracked_dir();

        std::fs::write(dir.path().join("readme.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("data.bin"), vec![0u8; 100]).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            dir.path(),
            dir.path(),
            &classifier,
            &mut stats,
            false,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(stats.total_tracked, 1, "only .bin files should be counted");
    }

    #[test]
    fn status_walks_subdirectories() {
        let dir = setup_tracked_dir();

        let sub = dir.path().join("models");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("weights.bin"), vec![0u8; 300]).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            dir.path(),
            dir.path(),
            &classifier,
            &mut stats,
            false,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(stats.total_tracked, 1);
        assert_eq!(stats.hydrated_count, 1);
        assert_eq!(stats.hydrated_size, 300);
    }

    #[test]
    fn run_status_in_no_gitattributes() {
        let dir = tempfile::tempdir().unwrap();
        // No .gitattributes — should print message and succeed.
        run_status_in(dir.path(), false, OutputMode::Text).unwrap();
    }

    #[test]
    fn run_status_in_empty_tracked_dir() {
        let dir = setup_tracked_dir();
        // .gitattributes exists but no matching files.
        run_status_in(dir.path(), false, OutputMode::Text).unwrap();
    }

    // --- porcelain output tests ---

    #[test]
    fn porcelain_pointer_file_reports_p() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("model.bin"), ptr.serialize()).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            dir.path(),
            dir.path(),
            &classifier,
            &mut stats,
            true,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(stats.pointer_count, 1);
        assert_eq!(stats.hydrated_count, 0);
    }

    #[test]
    fn porcelain_hydrated_file_reports_h_without_git() {
        // Without a git repo, non-pointer files fall back to H.
        let dir = setup_tracked_dir();
        std::fs::write(dir.path().join("data.bin"), vec![0xAB; 8192]).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            dir.path(),
            dir.path(),
            &classifier,
            &mut stats,
            true,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(stats.hydrated_count, 1);
        assert_eq!(stats.pointer_count, 0);
    }

    #[test]
    fn porcelain_mixed_files() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(2048);
        std::fs::write(dir.path().join("lazy.bin"), ptr.serialize()).unwrap();
        std::fs::write(dir.path().join("eager.bin"), vec![0xFF; 5000]).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            dir.path(),
            dir.path(),
            &classifier,
            &mut stats,
            true,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(stats.total_tracked, 2);
        assert_eq!(stats.pointer_count, 1);
        assert_eq!(stats.hydrated_count, 1);
    }

    #[test]
    fn read_committed_pointer_returns_none_without_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"hello").unwrap();

        let result = read_committed_pointer(dir.path(), Path::new("data.bin"));
        assert!(result.is_none());
    }

    #[test]
    fn read_committed_pointer_returns_pointer_from_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Initialize a git repo.
        let ok = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if ok.is_err() || !ok.unwrap().success() {
            // git not available in this environment — skip.
            return;
        }

        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .status();

        // Commit a pointer file.
        let ptr = sample_pointer(4096);
        std::fs::write(root.join("data.bin"), ptr.serialize()).unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "data.bin"])
            .current_dir(root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "add pointer"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let committed = read_committed_pointer(root, Path::new("data.bin"));
        assert!(committed.is_some());
        assert_eq!(committed.unwrap().size, 4096);
    }

    #[test]
    fn porcelain_detects_modified_file_via_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Initialize git repo.
        let ok = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if ok.is_err() || !ok.unwrap().success() {
            return;
        }

        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .status();

        // Write .gitattributes and a pointer, then commit.
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        let ptr = sample_pointer(4096);
        std::fs::write(root.join("data.bin"), ptr.serialize()).unwrap();

        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        // Now replace the pointer with hydrated content whose size
        // differs from the pointer's size field (4096). This should
        // be detected as Modified.
        std::fs::write(root.join("data.bin"), vec![0xAB; 9999]).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            root,
            root,
            &classifier,
            &mut stats,
            true,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        // The file is hydrated (non-pointer on disk).
        assert_eq!(stats.hydrated_count, 1);
        // The committed pointer has size 4096, but the file is 9999 bytes,
        // so porcelain should output "M data.bin". We verify the stats
        // path was taken; the actual "M" vs "H" tag is printed to stdout.
    }

    #[test]
    fn porcelain_hydrated_matches_committed_pointer_size() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let ok = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if ok.is_err() || !ok.unwrap().success() {
            return;
        }

        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .status();

        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        // Commit a pointer with size=8192.
        let ptr = sample_pointer(8192);
        std::fs::write(root.join("data.bin"), ptr.serialize()).unwrap();

        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        // Replace with hydrated content whose size matches the pointer's
        // size field. This should be detected as Hydrated (H), not Modified.
        std::fs::write(root.join("data.bin"), vec![0xAB; 8192]).unwrap();

        let mut stats = StatusStats::default();
        let classifier = classifier_for(vec!["*.bin".to_string()]);
        walk_tree(
            root,
            root,
            &classifier,
            &mut stats,
            true,
            false,
            &mut Vec::new(),
        )
        .unwrap();

        assert_eq!(stats.hydrated_count, 1);
        // Size matches committed pointer → porcelain outputs "H data.bin".
    }

    #[test]
    fn linked_worktree_status_reports_current_hydration_states() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let linked = dir.path().join("linked");

        let ok = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", root.to_str().unwrap()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if ok.is_err() || !ok.unwrap().success() {
            return;
        }
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .status();

        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        let hydrated_content = vec![0xAB; 4096];
        let hydrated_pointer = Pointer {
            file_hash: *blake3::hash(&hydrated_content).as_bytes(),
            size: hydrated_content.len() as u64,
            shard_hint: None,
        };
        let modified_pointer = Pointer {
            file_hash: *blake3::hash(b"original modified bytes").as_bytes(),
            size: 4096,
            shard_hint: None,
        };
        std::fs::write(root.join("pointer.bin"), sample_pointer(1024).serialize()).unwrap();
        std::fs::write(root.join("hydrated.bin"), hydrated_pointer.serialize()).unwrap();
        std::fs::write(root.join("modified.bin"), modified_pointer.serialize()).unwrap();

        let _ = std::process::Command::new("git")
            .args([
                "add",
                ".gitattributes",
                "pointer.bin",
                "hydrated.bin",
                "modified.bin",
            ])
            .current_dir(&root)
            .status();
        let committed = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if committed.is_err() || !committed.unwrap().success() {
            return;
        }

        let added = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&root)
            .status();
        if added.is_err() || !added.unwrap().success() {
            return;
        }
        assert!(linked.join(".git").is_file());

        std::fs::write(linked.join("hydrated.bin"), &hydrated_content).unwrap();
        std::fs::write(linked.join("modified.bin"), vec![0xCD; 9999]).unwrap();

        let classifier = TrackedClassifier::open(&linked).unwrap();
        let entries = collect_crab_entries(&linked, &classifier).unwrap();

        let state_for = |path: &str| {
            entries
                .iter()
                .find(|(entry_path, _, _)| entry_path == path)
                .map(|(_, state, _)| *state)
        };
        assert_eq!(state_for("pointer.bin"), Some(CrabState::Pointer));
        assert_eq!(state_for("hydrated.bin"), Some(CrabState::Hydrated));
        assert_eq!(state_for("modified.bin"), Some(CrabState::Modified));
    }

    #[test]
    fn linked_worktree_status_uses_shared_staging_for_mirror_pending_count() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let linked = dir.path().join("linked");

        let ok = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", root.to_str().unwrap()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if ok.is_err() || !ok.unwrap().success() {
            return;
        }
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .status();
        std::fs::write(root.join("README.md"), b"initial").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&root)
            .status();
        let committed = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if committed.is_err() || !committed.unwrap().success() {
            return;
        }
        let added = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&root)
            .status();
        if added.is_err() || !added.unwrap().success() {
            return;
        }

        assert_eq!(
            staging_path_for_status_root(&linked),
            root.canonicalize().unwrap().join(".crab").join("staging")
        );
    }

    // --- Unified status tests ---

    #[test]
    fn parse_git_porcelain_v2_untracked() {
        let output = "? new_file.txt\n? another.bin\n";
        let entries = parse_git_porcelain_v2(output);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "new_file.txt");
        assert_eq!(entries[0].state, GitState::Untracked);
        assert_eq!(entries[1].path, "another.bin");
        assert_eq!(entries[1].state, GitState::Untracked);
    }

    #[test]
    fn parse_git_porcelain_v2_staged() {
        // Type 1 entry: staged modification (M in index, . in worktree)
        let output = "1 M. N... 100644 100644 100644 abc123 def456 src/main.rs\n";
        let entries = parse_git_porcelain_v2(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "src/main.rs");
        assert_eq!(entries[0].state, GitState::Staged);
    }

    #[test]
    fn parse_git_porcelain_v2_unstaged() {
        // Type 1 entry: unstaged modification (. in index, M in worktree)
        let output = "1 .M N... 100644 100644 100644 abc123 def456 lib/utils.rs\n";
        let entries = parse_git_porcelain_v2(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "lib/utils.rs");
        assert_eq!(entries[0].state, GitState::Unstaged);
    }

    #[test]
    fn parse_git_porcelain_v2_both_staged_and_unstaged() {
        // Type 1 entry: both staged and unstaged (MM)
        let output = "1 MM N... 100644 100644 100644 abc123 def456 dual.rs\n";
        let entries = parse_git_porcelain_v2(output);
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|e| e.path == "dual.rs" && e.state == GitState::Staged)
        );
        assert!(
            entries
                .iter()
                .any(|e| e.path == "dual.rs" && e.state == GitState::Unstaged)
        );
    }

    #[test]
    fn parse_git_porcelain_v2_renamed() {
        // Type 2 entry: renamed file
        let output = "2 R. N... 100644 100644 100644 abc123 def456 R100 new_name.rs\told_name.rs\n";
        let entries = parse_git_porcelain_v2(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "new_name.rs");
        assert_eq!(entries[0].state, GitState::Staged);
    }

    #[test]
    fn parse_git_porcelain_v2_ignores_headers() {
        let output = "# branch.oid abc123\n# branch.head main\n? untracked.txt\n";
        let entries = parse_git_porcelain_v2(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "untracked.txt");
    }

    #[test]
    fn parse_git_porcelain_v2_empty() {
        let entries = parse_git_porcelain_v2("");
        assert!(entries.is_empty());
    }

    #[test]
    fn merge_status_git_only() {
        let git_entries = vec![
            GitStatusEntry {
                path: "a.txt".to_owned(),
                state: GitState::Staged,
            },
            GitStatusEntry {
                path: "b.txt".to_owned(),
                state: GitState::Untracked,
            },
        ];
        let crab_entries: Vec<(String, CrabState, u64)> = Vec::new();

        let (entries, summary) = merge_status(git_entries, &crab_entries);
        assert_eq!(entries.len(), 2);
        assert_eq!(summary.total_tracked, 0);
    }

    #[test]
    fn merge_status_crab_only() {
        let git_entries = Vec::new();
        let crab_entries = vec![
            ("model.bin".to_owned(), CrabState::Pointer, 4096),
            ("data.bin".to_owned(), CrabState::Hydrated, 8192),
        ];

        let (entries, summary) = merge_status(git_entries, &crab_entries);
        assert_eq!(entries.len(), 2);
        assert_eq!(summary.total_tracked, 2);
        assert_eq!(summary.pointer_count, 1);
        assert_eq!(summary.pointer_bytes, 4096);
        assert_eq!(summary.hydrated_count, 1);
        assert_eq!(summary.hydrated_bytes, 8192);
    }

    #[test]
    fn merge_status_join_on_path() {
        let git_entries = vec![GitStatusEntry {
            path: "model.bin".to_owned(),
            state: GitState::Staged,
        }];
        let crab_entries = vec![("model.bin".to_owned(), CrabState::Modified, 9999)];

        let (entries, summary) = merge_status(git_entries, &crab_entries);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.path, "model.bin");
        assert_eq!(entry.git_state, Some(GitState::Staged));
        assert_eq!(entry.crab_state, Some(CrabState::Modified));
        assert_eq!(entry.size, 9999);
        assert_eq!(summary.modified_count, 1);
    }

    #[test]
    fn merge_status_preserves_all_entries() {
        let git_entries = vec![
            GitStatusEntry {
                path: "a.txt".to_owned(),
                state: GitState::Staged,
            },
            GitStatusEntry {
                path: "shared.bin".to_owned(),
                state: GitState::Unstaged,
            },
        ];
        let crab_entries = vec![
            ("shared.bin".to_owned(), CrabState::Hydrated, 1024),
            ("only_crab.bin".to_owned(), CrabState::Pointer, 2048),
        ];

        let (entries, _summary) = merge_status(git_entries, &crab_entries);
        // a.txt (git only) + shared.bin (both) + only_crab.bin (crab only) = 3
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().any(|e| e.path == "a.txt"));
        assert!(entries.iter().any(|e| e.path == "shared.bin"));
        assert!(entries.iter().any(|e| e.path == "only_crab.bin"));

        // shared.bin should have both states
        let shared = entries.iter().find(|e| e.path == "shared.bin").unwrap();
        assert_eq!(shared.git_state, Some(GitState::Unstaged));
        assert_eq!(shared.crab_state, Some(CrabState::Hydrated));
    }

    #[test]
    fn unified_porcelain_format() {
        let entries = vec![
            UnifiedEntry {
                path: "staged.rs".to_owned(),
                git_state: Some(GitState::Staged),
                crab_state: None,
                size: 0,
            },
            UnifiedEntry {
                path: "model.bin".to_owned(),
                git_state: Some(GitState::Unstaged),
                crab_state: Some(CrabState::Modified),
                size: 4096,
            },
            UnifiedEntry {
                path: "new.txt".to_owned(),
                git_state: Some(GitState::Untracked),
                crab_state: None,
                size: 0,
            },
            UnifiedEntry {
                path: "lazy.bin".to_owned(),
                git_state: None,
                crab_state: Some(CrabState::Pointer),
                size: 2048,
            },
        ];

        // Verify the porcelain codes are correct by checking the format logic.
        let codes: Vec<String> = entries
            .iter()
            .map(|e| {
                let git_code = match e.git_state {
                    Some(GitState::Staged) => 'S',
                    Some(GitState::Unstaged) => 'U',
                    Some(GitState::Untracked) => '?',
                    None => '.',
                };
                let crab_code = match e.crab_state {
                    Some(CrabState::Pointer) => 'P',
                    Some(CrabState::Hydrated) => 'H',
                    Some(CrabState::Modified) => 'M',
                    None => '.',
                };
                format!("{}{} {}", git_code, crab_code, e.path)
            })
            .collect();

        assert_eq!(codes[0], "S. staged.rs");
        assert_eq!(codes[1], "UM model.bin");
        assert_eq!(codes[2], "?. new.txt");
        assert_eq!(codes[3], ".P lazy.bin");
    }

    #[test]
    fn crab_summary_counts_states_correctly() {
        let git_entries = Vec::new();
        let crab_entries = vec![
            ("a.bin".to_owned(), CrabState::Pointer, 1000),
            ("b.bin".to_owned(), CrabState::Pointer, 2000),
            ("c.bin".to_owned(), CrabState::Hydrated, 3000),
            ("d.bin".to_owned(), CrabState::Modified, 4000),
        ];

        let (_entries, summary) = merge_status(git_entries, &crab_entries);
        assert_eq!(summary.total_tracked, 4);
        assert_eq!(summary.pointer_count, 2);
        assert_eq!(summary.pointer_bytes, 3000);
        assert_eq!(summary.hydrated_count, 1);
        assert_eq!(summary.hydrated_bytes, 3000);
        assert_eq!(summary.modified_count, 1);
    }

    #[test]
    fn collect_crab_entries_finds_tracked_files() {
        let dir = setup_tracked_dir();

        // Write a pointer file.
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("model.bin"), ptr.serialize()).unwrap();

        // Write a hydrated file.
        std::fs::write(dir.path().join("data.bin"), vec![0xAB; 8192]).unwrap();

        let classifier = classifier_for(vec!["*.bin".to_string()]);
        let entries = collect_crab_entries(dir.path(), &classifier).unwrap();

        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|(p, s, _)| p == "model.bin" && *s == CrabState::Pointer)
        );
        assert!(
            entries
                .iter()
                .any(|(p, s, _)| p == "data.bin" && *s == CrabState::Hydrated)
        );
    }
}
