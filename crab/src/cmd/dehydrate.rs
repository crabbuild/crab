//! `crab dehydrate` — replace hydrated files with pointer blobs.
//!
//! Walks the working tree, identifies hydrated (non-pointer) files
//! matching the resolved pattern set, computes blake3 + size for each,
//! constructs the pointer blob, and atomically replaces the file.
//!
//! All writes go through a tempfile-then-rename pattern for crash
//! safety, identical to the hydrate command.

use std::collections::HashSet;
use std::io::{Read, Stdout, Write};
use std::path::{Path, PathBuf};
#[cfg(not(feature = "gix-worktree"))]
use std::process::Command;
use std::time::Instant;

use globset::GlobSet;
use schemars::JsonSchema;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::core::error::{self, Result};
use crate::core::output::event_payloads::FileDonePayload;
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::core::pattern::{PatternFilter, build_filter};
use crate::engine::pointer::is_working_tree_pointer;
use crab_types::pointer::Pointer;

/// Arguments for the dehydrate command.
#[derive(Debug, Clone)]
pub struct DehydrateArgs {
    /// Positional glob patterns to dehydrate.
    pub patterns: Vec<String>,
    /// Dehydrate all hydrated files.
    pub all: bool,
    /// Ignore prefetch profiles — dehydrate even files protected by the
    /// `always` profile.
    pub ignore_profiles: bool,
    /// Output mode resolved from `--json` / `--jsonl` flags.
    pub mode: OutputMode,
}

/// Summary of a batch dehydration run.
#[derive(Debug, Clone, Default)]
pub struct DehydrateSummary {
    /// Number of files successfully dehydrated.
    pub dehydrated: u64,
    /// Total bytes freed (original file sizes minus pointer sizes).
    pub bytes_freed: u64,
    /// Number of files skipped (already pointers).
    pub skipped: u64,
    /// Number of files that failed to dehydrate.
    pub failed: u64,
    /// Number of dirty files skipped to avoid data loss.
    pub dirty_skipped: u64,
    /// Number of files protected by the `always` prefetch profile.
    pub profile_protected: u64,
}

/// Serializable payload for `--json` / `--jsonl` result events.
#[derive(Debug, Serialize, JsonSchema)]
pub struct DehydrateSummaryPayload {
    pub dehydrated: u64,
    pub bytes_freed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub dirty_skipped: u64,
    pub profile_protected: u64,
    pub duration_ms: u64,
}

impl DehydrateSummaryPayload {
    fn from_summary(summary: &DehydrateSummary, elapsed: std::time::Duration) -> Self {
        Self {
            dehydrated: summary.dehydrated,
            bytes_freed: summary.bytes_freed,
            skipped: summary.skipped,
            failed: summary.failed,
            dirty_skipped: summary.dirty_skipped,
            profile_protected: summary.profile_protected,
            duration_ms: elapsed.as_millis() as u64,
        }
    }
}

/// Run the dehydrate command.
///
/// Resolves effective patterns, walks the working tree, and replaces
/// hydrated files with their pointer blobs.
///
/// # Errors
///
/// Returns [`CrabError::InvalidPattern`] on bad globs,
/// [`CrabError::Io`] on filesystem failures, or
/// [`CrabError::Cancelled`] if the token fires mid-walk.
pub fn run_dehydrate(args: &DehydrateArgs, cancel: &CancellationToken) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let root =
        crate::git::worktree::WorktreeContext::resolve_from_path(&cwd)?.current_worktree_root;
    run_dehydrate_in(&root, args, cancel)
}

/// Dehydrate implementation that accepts an explicit root directory.
pub fn run_dehydrate_in(
    root: &Path,
    args: &DehydrateArgs,
    cancel: &CancellationToken,
) -> Result<()> {
    let filter = resolve_patterns(args)?;

    let Some(filter) = filter else {
        if !args.mode.is_machine() {
            print_help();
        }
        return Ok(());
    };

    let tracked = TrackedClassifier::open(root)?;
    if tracked.is_empty() {
        if !args.mode.is_machine() {
            println!("No crab-tracked patterns found in .gitattributes");
        }
        return Ok(());
    }

    debug!("loaded crab-tracked attributes");

    let mut to_dehydrate: Vec<PathBuf> = Vec::new();
    walk_hydrated_files(root, root, &tracked, &filter, cancel, &mut to_dehydrate)?;

    if to_dehydrate.is_empty() {
        if !args.mode.is_machine() {
            println!("No hydrated files match the given patterns.");
        }
        return Ok(());
    }

    // Build the set of paths protected by the `always` prefetch profile.
    let protected = build_profile_protection(root, args.ignore_profiles);

    // Filter out profile-protected files before dehydrating.
    let (to_dehydrate, profile_protected_count) = if let Some(ref glob_set) = protected {
        let mut kept = Vec::new();
        let mut protected_count: u64 = 0;
        for path in to_dehydrate {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if glob_set.is_match(rel) {
                debug!(path = %rel.display(), "protected by always profile, skipping");
                protected_count += 1;
            } else {
                kept.push(path);
            }
        }
        if protected_count > 0 {
            info!(count = protected_count, "files protected by always profile");
        }
        (kept, protected_count)
    } else {
        (to_dehydrate, 0)
    };

    if to_dehydrate.is_empty() {
        if !args.mode.is_machine() {
            if profile_protected_count > 0 {
                println!(
                    "All matching files are protected by the always prefetch profile. \
                     Use --ignore-profiles to override."
                );
            } else {
                println!("No hydrated files match the given patterns.");
            }
        }
        return Ok(());
    }

    info!(count = to_dehydrate.len(), "dehydrating files");

    let dirty = query_dirty_files(root);

    // Build the optional JSONL stream for streaming mode.
    let jsonl_stream: Option<std::sync::Mutex<JsonlStream<Stdout>>> = match args.mode {
        OutputMode::Jsonl => Some(std::sync::Mutex::new(JsonlStream::new(
            "dehydrate.event",
            "1.0",
            std::io::stdout(),
        ))),
        _ => None,
    };

    let start = Instant::now();
    let (mut summary, first_failure) =
        dehydrate_batch_with_events(root, &to_dehydrate, &dirty, cancel, jsonl_stream.as_ref())?;
    let elapsed = start.elapsed();

    summary.profile_protected = profile_protected_count;

    // Invalidate hydrated-pointer cache entries for every dehydrated
    // path so the clean filter no longer short-circuits on a stale
    // fingerprint. Best-effort: a failure here leaves stale entries
    // that will be invalidated lazily on the next clean (stat
    // mismatch detection).
    if summary.dehydrated > 0 {
        let invalidations: Vec<String> = to_dehydrate
            .iter()
            .map(|path| {
                let rel = path.strip_prefix(root).unwrap_or(path);
                rel.to_string_lossy().replace('\\', "/")
            })
            .collect();
        match crate::cache::hydrated_pointer::cache_path_for_worktree_root(root) {
            Ok(cache_path) => {
                if let Err(e) = crate::cache::HydratedPointerCache::invalidate_on_disk(
                    &cache_path,
                    invalidations,
                ) {
                    debug!(
                        path = %cache_path.display(),
                        error = %e,
                        "failed to invalidate hydrated-pointer cache (non-fatal)"
                    );
                }
            }
            Err(e) => {
                debug!(
                    root = %root.display(),
                    error = %e,
                    "hydrated-pointer cache unavailable for dehydrate invalidation"
                );
            }
        }
    }

    let payload = DehydrateSummaryPayload::from_summary(&summary, elapsed);

    match args.mode {
        OutputMode::Text => {
            println!(
                "Dehydrated {} file(s), freed {} in {}",
                summary.dehydrated,
                format_bytes(summary.bytes_freed),
                format_elapsed(elapsed),
            );
            if summary.skipped > 0 {
                println!("{} already dehydrated, skipped", summary.skipped);
            }
            if summary.dirty_skipped > 0 {
                println!(
                    "{} dirty file(s) skipped (uncommitted changes)",
                    summary.dirty_skipped,
                );
            }
            if summary.profile_protected > 0 {
                println!(
                    "{} file(s) protected by always profile (use --ignore-profiles to override)",
                    summary.profile_protected,
                );
            }
            if summary.failed > 0 {
                println!("{} failed", summary.failed);
            }
        }
        OutputMode::Json => {
            emit_json("dehydrate", "1.0", &payload);
        }
        OutputMode::Jsonl => {
            if let Some(stream) = &jsonl_stream
                && let Ok(mut s) = stream.lock()
            {
                s.emit_result(&payload);
            }
        }
    }

    if let Some(error) = first_failure {
        return Err(error);
    }

    Ok(())
}

/// Resolve effective patterns from args.
///
/// Priority: `--all` > positional globs > `None` (print help).
fn resolve_patterns(args: &DehydrateArgs) -> Result<Option<PatternFilter>> {
    if args.all {
        let filter = build_filter(&["**/*".to_owned()], &[])?;
        return Ok(Some(filter));
    }

    if !args.patterns.is_empty() {
        let filter = build_filter(&args.patterns, &[])?;
        return Ok(Some(filter));
    }

    Ok(None)
}

/// Build a [`GlobSet`] from the `always` prefetch profile, if present.
///
/// Returns `None` when `--ignore-profiles` is set, the prefetch file
/// doesn't exist, or the `always` profile has no patterns. If loading
/// or parsing the prefetch file fails, logs a warning and returns `None`
/// so dehydrate is never blocked by a broken config.
fn build_profile_protection(root: &Path, ignore_profiles: bool) -> Option<GlobSet> {
    if ignore_profiles {
        debug!("--ignore-profiles set, skipping profile protection");
        return None;
    }

    let config = match crate::hydrate::profile::load_prefetch(root) {
        Ok(c) => c,
        Err(e) => {
            warn!(err = %e, "failed to load prefetch config, proceeding without profile protection");
            return None;
        }
    };

    let globs = config.profiles.get("always")?;
    if globs.is_empty() {
        return None;
    }

    let mut builder = globset::GlobSetBuilder::new();
    for glob in globs {
        builder.add(glob.clone());
    }

    match builder.build() {
        Ok(set) => {
            debug!(
                patterns = globs.len(),
                "built always-profile protection set"
            );
            Some(set)
        }
        Err(e) => {
            warn!(err = %e, "failed to compile always-profile globs, proceeding without protection");
            None
        }
    }
}

/// Print a help message when no patterns are provided.
fn print_help() {
    eprintln!("Usage: crab dehydrate <glob>... [--all]");
    eprintln!();
    eprintln!("Replace hydrated files with their pointer blobs, freeing disk space.");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  crab dehydrate \"*.safetensors\"");
    eprintln!("  crab dehydrate \"models/**\"");
    eprintln!("  crab dehydrate --all");
}

/// Per-site classifier for `.gitattributes filter=crab` lookup.
///
/// Under `gix-pathmatch`, wraps the consolidated
/// [`core::attrs::TrackedClassifier`] (backed by `gix_attributes::Search`).
/// Otherwise falls back to the legacy suffix-matching helper driven by
/// patterns parsed out of the root `.gitattributes` line-by-line.
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
            false
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            self.patterns.is_empty()
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
            matches_any_tracked_legacy(rel_path, &self.patterns)
        }
    }
}

/// Legacy fallback for builds without `gix-pathmatch`. The consolidated
/// matcher lives in `core::attrs`.
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
        .filter_map(|line| line.split_whitespace().next().map(String::from))
        .collect();

    Ok(globs)
}

/// Legacy suffix-matching helper retained for builds without `gix-pathmatch`.
#[cfg(not(feature = "gix-pathmatch"))]
fn matches_any_tracked_legacy(rel_path: &Path, patterns: &[String]) -> bool {
    let path_str = rel_path.to_string_lossy();

    for pattern in patterns {
        if pattern == "*" || pattern == "**" || pattern == "**/*" {
            return true;
        }
        if let Some(suffix) = pattern.strip_prefix('*')
            && path_str.ends_with(suffix)
        {
            return true;
        }
        if *pattern == *path_str {
            return true;
        }
    }
    false
}

/// Recursively walk the directory tree, collecting hydrated (non-pointer)
/// files that match both the tracked patterns and the user's dehydrate filter.
fn walk_hydrated_files(
    root: &Path,
    dir: &Path,
    tracked: &TrackedClassifier,
    filter: &PatternFilter,
    cancel: &CancellationToken,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    error::check_cancelled(cancel)?;

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
            walk_hydrated_files(root, &path, tracked, filter, cancel, out)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let Ok(rel_path) = path.strip_prefix(root) else {
            continue;
        };

        // Must be a crab-tracked file.
        if !tracked.is_tracked(rel_path) {
            continue;
        }

        let rel_str = rel_path.to_string_lossy();

        // Must match the user's dehydrate pattern filter.
        if !filter.matches(&rel_str) {
            continue;
        }

        // Must be hydrated (NOT a pointer). Skip files already in pointer state.
        match is_working_tree_pointer(&path) {
            Ok(true) => continue, // already a pointer, skip
            Ok(false) => {}       // hydrated, collect it
            Err(e) => {
                debug!(path = %path.display(), err = %e, "skipping unreadable file");
                continue;
            }
        }

        out.push(path.clone());
    }

    Ok(())
}

/// Query dirty paths in the working tree (modified or untracked).
///
/// Returns a set of repo-relative paths that are dirty (modified in
/// the index vs HEAD, modified in the worktree vs index, or
/// untracked). Returns an empty set when no git repo is present or
/// any step fails — the safety check is best-effort.
///
/// Under the `gix-worktree` feature this walks the index + HEAD tree
/// via `gix-index` + `gix-odb` + `gix-dir` entirely in-process,
/// eliminating the per-invocation `fork+exec+git status --porcelain`
/// shellout. On a 100 k-file tree the speedup is orders of magnitude
/// per Task 7.10.
#[cfg(feature = "gix-worktree")]
fn query_dirty_files(root: &Path) -> HashSet<String> {
    match query_dirty_files_via_gix(root) {
        Ok(set) => set,
        Err(e) => {
            debug!(err = %e, "gix-native dirty-check failed, returning empty set");
            HashSet::new()
        }
    }
}

/// Legacy shellout path for builds without `gix-worktree`.
///
/// Returns a set of repo-relative paths with uncommitted changes
/// (modified, added, untracked, etc.). If `git` is not available or
/// the directory is not a git repo, returns an empty set — the
/// safety check is best-effort.
///
/// Hydrated files (worktree has full content, index has a crab
/// pointer) are excluded from the dirty set. From the user's
/// perspective these are not "dirty" — they are the expected result
/// of `crab hydrate` and dehydrate must be able to reverse them.
#[cfg(not(feature = "gix-worktree"))]
fn query_dirty_files(root: &Path) -> HashSet<String> {
    let mut dirty = HashSet::new();

    let Some(modified) = git_lines(root, &["ls-files", "-m"]) else {
        debug!("git ls-files unavailable, skipping dirty-file check");
        return dirty;
    };
    for path in modified {
        if !index_blob_is_pointer(root, &path) {
            dirty.insert(path);
        }
    }

    for args in [
        &["ls-files", "-d"][..],
        &["ls-files", "-o", "--exclude-standard"][..],
        &["diff", "--cached", "--name-only", "--no-ext-diff"][..],
    ] {
        if let Some(paths) = git_lines(root, args) {
            dirty.extend(paths);
        }
    }

    dirty
}

#[cfg(not(feature = "gix-worktree"))]
fn git_lines(root: &Path, args: &[&str]) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    )
}

/// In-process dirty-file probe using `gix-index` and `gix-odb`.
///
/// The implementation is deliberately narrower than
/// `gix_status::index_as_worktree`: callers here only need a
/// `HashSet<String>` of "don't dehydrate these paths", so we compute
/// three contributions and union them:
///
/// 1. **Unstaged modifications** — index OID != worktree-content OID.
///    Computed by hashing each worktree file and comparing against
///    the index entry's OID. Missing worktree files count as dirty
///    (deleted; dehydrating would have nothing to do anyway).
/// 2. **Staged modifications** — index OID != HEAD tree OID at the
///    same path. Skipped when HEAD doesn't resolve (brand-new repo
///    with no commits — everything is staged by definition).
/// 3. **Untracked files** — worktree paths that are not in the
///    index at all. Walked via a simple recursive scan under
///    `root`, ignoring `.git` and `.crab` internals.
///
/// Rename detection is intentionally omitted. A rename from the
/// caller's perspective is a delete + add on either side of the
/// status; the resulting paths land in the set either way.
#[cfg(feature = "gix-worktree")]
fn query_dirty_files_via_gix(root: &Path) -> Result<HashSet<String>> {
    use gix_object::FindExt;

    let mut dirty: HashSet<String> = HashSet::new();
    let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(root)?;

    // Open index. Non-existent index is treated as "no tracked files"
    // - everything in the worktree is untracked.
    let index_path = ctx.index_path();
    let index = if index_path.is_file() {
        Some(
            gix_index::File::at(
                &index_path,
                gix_hash::Kind::Sha1,
                true,
                gix_index::decode::Options::default(),
            )
            .map_err(|e| error::CrabError::Internal(format!("read index: {e}")))?,
        )
    } else {
        None
    };

    // Open ODB once — used for HEAD tree comparison and pointer detection.
    let odb = gix_odb::at(ctx.objects_dir())
        .map_err(|e| error::CrabError::Internal(format!("open odb: {e}")))?;

    // Optional HEAD → tree for staged comparisons.
    let head_tree_map =
        match crab_git::ref_resolve::resolve_ref_at(&ctx.per_worktree_git_dir, "HEAD") {
            Ok(sha) => {
                let Ok(commit_oid) = gix_hash::ObjectId::from_hex(sha.as_bytes()) else {
                    return Ok(dirty);
                };
                let mut buf = Vec::new();
                let Ok(commit) = odb.find_commit(&commit_oid, &mut buf) else {
                    return Ok(dirty);
                };
                let tree_oid = commit.tree();
                flatten_head_tree(&odb, &tree_oid).unwrap_or_default()
            }
            Err(_) => std::collections::HashMap::new(),
        };

    let mut indexed_paths: HashSet<String> = HashSet::new();

    // Passes (1) and (2) — walk the index.
    if let Some(ref index) = index {
        for entry in index.entries() {
            if entry.stage_raw() != 0 {
                continue;
            }
            let path_bytes = entry.path(index);
            let Ok(path) = std::str::from_utf8(path_bytes) else {
                continue;
            };
            indexed_paths.insert(path.to_owned());

            let abs = root.join(path);
            if index_entry_is_pointer(&odb, &entry.id) {
                // The index pointer already tells us this path may be hydrated;
                // dehydrate_one performs the Blake3 match before replacement.
                if !abs.is_file() {
                    dirty.insert(path.to_owned());
                    continue;
                }
            } else {
                // (1) worktree modifications — hash the working-tree file
                // and compare to the index OID. A missing file counts as
                // dirty (deletion).
                match std::fs::read(&abs) {
                    Ok(content) => {
                        if let Ok(worktree_oid) = gix_object::compute_hash(
                            gix_hash::Kind::Sha1,
                            gix_object::Kind::Blob,
                            &content,
                        ) {
                            if worktree_oid != entry.id {
                                dirty.insert(path.to_owned());
                                continue;
                            }
                        }
                    }
                    Err(_) => {
                        dirty.insert(path.to_owned());
                        continue;
                    }
                }
            }

            // (2) staged modifications — index vs HEAD.
            if let Some(head_oid) = head_tree_map.get(path) {
                if *head_oid != entry.id.to_hex().to_string() {
                    dirty.insert(path.to_owned());
                }
            } else {
                // Path is in the index but not in HEAD → staged addition.
                if !head_tree_map.is_empty() {
                    dirty.insert(path.to_owned());
                }
            }
        }
    }

    // (3) untracked files — walk the worktree and flag anything not
    // in `indexed_paths`. We stop at `.git` / `.crab` and hidden
    // directories to keep parity with the legacy shellout path, which
    // deferred to `git status`'s default excludes.
    walk_untracked(root, root, &indexed_paths, &mut dirty);

    Ok(dirty)
}

#[cfg(feature = "gix-worktree")]
fn index_entry_is_pointer(odb: &gix_odb::Handle, oid: &gix_hash::oid) -> bool {
    use gix_object::FindExt;

    let mut blob_buf = Vec::new();
    odb.find_blob(oid, &mut blob_buf)
        .map(|blob| crab_types::pointer::is_pointer(blob.data))
        .unwrap_or(false)
}

/// Build a `path -> oid_hex` map for every blob reachable from the
/// HEAD tree. Uses `gix_traverse::tree::breadthfirst` so sub-trees
/// are traversed in one pass. Missing OR corrupt tree objects
/// collapse to an empty map; callers fall through without staged-vs-
/// HEAD diffing on that pass.
#[cfg(feature = "gix-worktree")]
fn flatten_head_tree(
    odb: &gix_odb::Handle,
    tree_oid: &gix_hash::oid,
) -> Option<std::collections::HashMap<String, String>> {
    use gix_object::FindExt;

    let mut buf = Vec::new();
    let tree = odb.find_tree_iter(tree_oid, &mut buf).ok()?;

    let mut recorder = gix_traverse::tree::Recorder::default();
    let mut state = gix_traverse::tree::breadthfirst::State::default();
    gix_traverse::tree::breadthfirst(tree, &mut state, odb, &mut recorder).ok()?;

    let mut out = std::collections::HashMap::new();
    for record in recorder.records {
        use gix_object::tree::EntryKind;
        if !matches!(
            record.mode.kind(),
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
        ) {
            continue;
        }
        if let Ok(path) = std::str::from_utf8(&record.filepath) {
            out.insert(path.to_owned(), record.oid.to_hex().to_string());
        }
    }
    Some(out)
}

/// Recursively collect untracked worktree files into `dirty`.
///
/// Matches the legacy shellout's defaults: skip `.git`, `.crab`,
/// and dotfiles at the root (git's default exclude behavior). Parses
/// `.gitignore` files is not attempted here — dehydrate's dirty-check
/// is already a best-effort safety net, and pulling in the ignore
/// stack for this single use would add a lot of surface area for
/// marginal gain. Files that were intentionally left ignored by git
/// still end up in the dirty set, which only means dehydrate skips
/// them; that is the safer direction.
#[cfg(feature = "gix-worktree")]
fn walk_untracked(root: &Path, dir: &Path, indexed: &HashSet<String>, dirty: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk_untracked(root, &path, indexed, dirty);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        // Git paths are forward-slash normalized; do the same so
        // comparisons against the index and HEAD match.
        let rel_str = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if !indexed.contains(&rel_str) {
            dirty.insert(rel_str);
        }
    }
}

/// Check if the index blob for a given repo-relative path is a crab pointer.
///
/// Used by the dirty-file filter to distinguish hydrated files (index has
/// pointer, worktree has full content) from genuinely user-modified files.
/// Returns `false` on any failure (git unavailable, path not tracked, etc.)
/// — the conservative direction is to treat the file as dirty.
#[cfg(not(feature = "gix-worktree"))]
fn index_blob_is_pointer(root: &Path, rel_path: &str) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("show")
        .arg(format!(":{rel_path}"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();

    match output {
        Ok(o) if o.status.success() => crab_types::pointer::is_pointer(&o.stdout),
        _ => false,
    }
}

/// Dehydrate a batch of hydrated files by computing blake3 + size and
/// writing pointer blobs atomically. Skips files with uncommitted changes.
#[cfg(test)]
fn dehydrate_batch(
    root: &Path,
    files: &[PathBuf],
    dirty: &HashSet<String>,
    cancel: &CancellationToken,
) -> Result<DehydrateSummary> {
    let mut summary = DehydrateSummary::default();

    for path in files {
        error::check_cancelled(cancel)?;

        // Check if the file is dirty — refuse to dehydrate to avoid data loss.
        if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = rel.to_string_lossy();
            if dirty.contains(rel_str.as_ref()) {
                warn!(path = %rel.display(), "skipping dirty file");
                eprintln!("warning: skipping dirty file: {}", rel.display());
                summary.dirty_skipped += 1;
                continue;
            }
        }

        match dehydrate_one(path, Some(root)) {
            Ok((original_size, pointer_size)) => {
                summary.dehydrated += 1;
                summary.bytes_freed += original_size.saturating_sub(pointer_size);
            }
            Err(e) => {
                debug!(path = %path.display(), err = %e, "failed to dehydrate file");
                summary.failed += 1;
            }
        }
    }

    Ok(summary)
}

/// Dehydrate a batch with optional JSONL `file_done` event emission.
fn dehydrate_batch_with_events(
    root: &Path,
    files: &[PathBuf],
    dirty: &HashSet<String>,
    cancel: &CancellationToken,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
) -> Result<(DehydrateSummary, Option<error::CrabError>)> {
    let mut summary = DehydrateSummary::default();
    let mut first_failure = None;

    for path in files {
        error::check_cancelled(cancel)?;

        let rel = path.strip_prefix(root).unwrap_or(path);
        let rel_string = rel.to_string_lossy().into_owned();

        // Check if the file is dirty — refuse to dehydrate to avoid data loss.
        if dirty.contains(&rel_string) {
            warn!(path = %rel.display(), "skipping dirty file");
            if jsonl_stream.is_none() {
                eprintln!("warning: skipping dirty file: {}", rel.display());
            }
            summary.dirty_skipped += 1;
            if let Some(stream) = jsonl_stream
                && let Ok(mut s) = stream.lock()
            {
                s.emit_file_done(FileDonePayload {
                    path: rel_string,
                    bytes: 0,
                    duration_ms: 0,
                    status: "skipped".to_owned(),
                });
            }
            continue;
        }

        let file_start = Instant::now();
        match dehydrate_one(path, Some(root)) {
            Ok((original_size, pointer_size)) => {
                summary.dehydrated += 1;
                summary.bytes_freed += original_size.saturating_sub(pointer_size);
                if let Some(stream) = jsonl_stream
                    && let Ok(mut s) = stream.lock()
                {
                    s.emit_file_done(FileDonePayload {
                        path: rel_string,
                        bytes: original_size,
                        duration_ms: file_start.elapsed().as_millis() as u64,
                        status: "ok".to_owned(),
                    });
                }
            }
            Err(e) => {
                debug!(path = %path.display(), err = %e, "failed to dehydrate file");
                summary.failed += 1;
                if first_failure.is_none() {
                    first_failure = Some(e);
                }
                if let Some(stream) = jsonl_stream
                    && let Ok(mut s) = stream.lock()
                {
                    s.emit_file_done(FileDonePayload {
                        path: rel_string,
                        bytes: 0,
                        duration_ms: file_start.elapsed().as_millis() as u64,
                        status: "failed".to_owned(),
                    });
                }
            }
        }
    }

    Ok((summary, first_failure))
}

/// Dehydrate a single file by streaming content through Blake3, verifying
/// the hash against the pointer committed to git, then atomically replacing
/// the file with a pointer blob.
///
/// Returns `(original_size, pointer_size)` on success.
///
/// # Safety
///
/// Before overwriting a file in a Git worktree, this function verifies that
/// the computed blake3 hash matches the file's pointer in the current commit.
/// If the hash doesn't match, the file has been modified relative to its
/// committed pointer — dehydrating would produce a pointer that no shard
/// can resolve, causing data loss when the user tries to hydrate it back.
///
/// The `repo_root` parameter (if `Some`) enables this verification. Callers
/// without access to git should pass `None` only when another owner proves
/// that the resulting pointer is reconstructable.
fn dehydrate_one(path: &Path, repo_root: Option<&Path>) -> Result<(u64, u64)> {
    let (file_hash, original_size) = hash_file(path)?;

    // Safety check: verify the computed hash matches the pointer stored
    // in the current commit. If git has a different pointer for this path, the
    // working-tree content diverges from what was committed, and
    // dehydrating it would orphan the content behind an unresolvable
    // pointer. The dirty-file check upstream normally filters these out,
    // but that check is best-effort (empty set if git is unavailable).
    //
    // We also preserve the shard_hint from the committed pointer so
    // subsequent hydration uses the fast path. Without this, dehydrate
    // would drop the hint and every hydrate would fall through to the
    // file-index lookup.
    let mut preserved_shard_hint: Option<[u8; 32]> = None;
    if let Some(root) = repo_root {
        match read_committed_pointer(root, path)? {
            Some(committed) if committed.file_hash == file_hash => {
                preserved_shard_hint = committed.shard_hint;
            }
            Some(committed) => {
                return Err(error::CrabError::Internal(format!(
                    "dehydrate safety: working-tree content at {} has hash {} \
                     but the committed pointer references {}. File was modified \
                     after commit; dehydrating would produce an unresolvable \
                     pointer. Commit changes first, or discard it with \
                     `git restore {}`.",
                    path.display(),
                    crab_types::pointer::hex_encode(&file_hash),
                    crab_types::pointer::hex_encode(&committed.file_hash),
                    path.display(),
                )));
            }
            None if root.join(".git").exists() => {
                return Err(error::CrabError::Internal(format!(
                    "dehydrate safety: committed Git blob for {} is not a Crab pointer. \
                     The file cannot be proven reconstructable; add and push it \
                     with Crab before dehydrating.",
                    path.display(),
                )));
            }
            None => {}
        }
    }

    let pointer = Pointer {
        file_hash,
        size: original_size,
        shard_hint: preserved_shard_hint,
    };

    let pointer_bytes = pointer.serialize();
    let pointer_size = pointer_bytes.len() as u64;

    atomic_write(path, &pointer_bytes)?;

    debug!(
        path = %path.display(),
        size = original_size,
        "dehydrated file"
    );

    Ok((original_size, pointer_size))
}

fn hash_file(path: &Path) -> Result<([u8; 32], u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut size = 0u64;

    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        size += read as u64;
    }

    Ok((*hasher.finalize().as_bytes(), size))
}

/// Read the file's pointer from the current commit (`git show HEAD:path`) and
/// return the committed file_hash plus optional shard_hint if it parses
/// as a crab pointer.
///
/// Returns `None` if git is unavailable, the path isn't tracked, or the
/// committed blob isn't a crab pointer. The caller should treat this
/// conservatively — missing info means we can't verify, not that the
/// file is safe to dehydrate.
fn read_committed_pointer(repo_root: &Path, path: &Path) -> Result<Option<Pointer>> {
    let rel = path.strip_prefix(repo_root).map_err(|_| {
        error::CrabError::Internal(format!(
            "dehydrate safety: {} is outside worktree {}",
            path.display(),
            repo_root.display()
        ))
    })?;
    let rel_str = rel.to_str().ok_or_else(|| {
        error::CrabError::Internal(format!(
            "dehydrate safety: path is not valid UTF-8: {}",
            rel.display()
        ))
    })?;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("show")
        .arg(format!("HEAD:{rel_str}"))
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let Ok(ptr) = Pointer::parse(&output.stdout) else {
        return Ok(None);
    };
    Ok(Some(ptr))
}

/// Write `content` to `dest` atomically via a sibling tempfile and rename.
fn atomic_write(dest: &Path, content: &[u8]) -> Result<()> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(content)?;
    tmp.flush()?;
    preserve_destination_permissions(dest, tmp.as_file())?;
    tmp.persist(dest).map_err(|e| e.error)?;
    Ok(())
}

fn preserve_destination_permissions(dest: &Path, temporary: &std::fs::File) -> Result<()> {
    match std::fs::metadata(dest) {
        Ok(metadata) => temporary
            .set_permissions(metadata.permissions())
            .map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Format a byte count as a human-readable string with binary units.
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

/// Format a duration as `Xm Ys` or `Xs` for short durations.
fn format_elapsed(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

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
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        dir
    }

    fn default_args() -> DehydrateArgs {
        DehydrateArgs {
            patterns: Vec::new(),
            all: false,
            ignore_profiles: false,
            mode: OutputMode::Text,
        }
    }

    // --- resolve_patterns tests ---

    #[test]
    fn resolve_all_matches_everything() {
        let args = DehydrateArgs {
            all: true,
            ..default_args()
        };
        let filter = resolve_patterns(&args).unwrap().unwrap();
        assert!(filter.matches("any/path/file.bin"));
    }

    #[test]
    fn resolve_positional_globs() {
        let args = DehydrateArgs {
            patterns: vec!["*.bin".to_owned()],
            ..default_args()
        };
        let filter = resolve_patterns(&args).unwrap().unwrap();
        assert!(filter.matches("model.bin"));
        assert!(!filter.matches("model.txt"));
    }

    #[test]
    fn resolve_returns_none_when_nothing_specified() {
        let args = default_args();
        assert!(resolve_patterns(&args).unwrap().is_none());
    }

    // --- dehydrate_one tests ---

    #[test]
    fn dehydrate_one_creates_valid_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        // Use content larger than a pointer (~120 bytes) so bytes_freed > 0.
        let content = vec![0xCD; 4096];
        std::fs::write(&path, &content).unwrap();

        let (original_size, pointer_size) = dehydrate_one(&path, None).unwrap();
        assert_eq!(original_size, 4096);
        assert!(pointer_size < original_size);

        // Verify the written file is a valid pointer.
        let written = std::fs::read(&path).unwrap();
        let ptr = Pointer::parse(&written).unwrap();
        assert_eq!(ptr.size, 4096);

        // Verify the hash matches blake3 of original content.
        let expected_hash = blake3::hash(&content);
        assert_eq!(ptr.file_hash, *expected_hash.as_bytes());
    }

    #[test]
    fn dehydrate_one_pointer_round_trips_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.bin");
        let content = vec![0xAB; 4096];
        std::fs::write(&path, &content).unwrap();

        dehydrate_one(&path, None).unwrap();

        let written = std::fs::read(&path).unwrap();
        let ptr = Pointer::parse(&written).unwrap();
        assert_eq!(ptr.size, 4096);

        let expected_hash = blake3::hash(&content);
        assert_eq!(ptr.file_hash, *expected_hash.as_bytes());
    }

    // --- dehydrate_batch tests ---

    #[test]
    fn dehydrate_batch_processes_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.bin");
        let path_b = dir.path().join("b.bin");
        std::fs::write(&path_a, vec![0xAA; 1024]).unwrap();
        std::fs::write(&path_b, vec![0xBB; 2048]).unwrap();

        let files = vec![path_a.clone(), path_b.clone()];
        let cancel = CancellationToken::new();
        let dirty = HashSet::new();
        let summary = dehydrate_batch(dir.path(), &files, &dirty, &cancel).unwrap();

        assert_eq!(summary.dehydrated, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.dirty_skipped, 0);
        assert!(summary.bytes_freed > 0);

        // Both files should now be valid pointers.
        assert!(is_working_tree_pointer(&path_a).unwrap());
        assert!(is_working_tree_pointer(&path_b).unwrap());
    }

    #[test]
    fn dehydrate_batch_respects_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        std::fs::write(&path, vec![0xAA; 1024]).unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();

        let files = vec![path];
        let dirty = HashSet::new();
        let result = dehydrate_batch(dir.path(), &files, &dirty, &cancel);
        assert!(result.is_err());
    }

    #[test]
    fn dehydrate_batch_skips_dirty_files() {
        let dir = tempfile::tempdir().unwrap();
        let path_clean = dir.path().join("clean.bin");
        let path_dirty = dir.path().join("dirty.bin");
        std::fs::write(&path_clean, vec![0xAA; 1024]).unwrap();
        std::fs::write(&path_dirty, vec![0xBB; 2048]).unwrap();

        let files = vec![path_clean.clone(), path_dirty.clone()];
        let cancel = CancellationToken::new();
        let dirty: HashSet<String> = ["dirty.bin".to_owned()].into_iter().collect();
        let summary = dehydrate_batch(dir.path(), &files, &dirty, &cancel).unwrap();

        assert_eq!(summary.dehydrated, 1);
        assert_eq!(summary.dirty_skipped, 1);

        // Clean file should be a pointer now.
        assert!(is_working_tree_pointer(&path_clean).unwrap());
        // Dirty file should remain hydrated.
        assert!(!is_working_tree_pointer(&path_dirty).unwrap());
    }

    #[test]
    fn dehydrate_batch_all_dirty_dehydrates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, vec![0xAA; 1024]).unwrap();

        let files = vec![path.clone()];
        let cancel = CancellationToken::new();
        let dirty: HashSet<String> = ["data.bin".to_owned()].into_iter().collect();
        let summary = dehydrate_batch(dir.path(), &files, &dirty, &cancel).unwrap();

        assert_eq!(summary.dehydrated, 0);
        assert_eq!(summary.dirty_skipped, 1);
        assert!(!is_working_tree_pointer(&path).unwrap());
    }

    // --- query_dirty_files tests ---

    #[test]
    fn query_dirty_files_returns_empty_without_git() {
        let dir = tempfile::tempdir().unwrap();
        let dirty = query_dirty_files(dir.path());
        assert!(dirty.is_empty());
    }

    #[test]
    fn query_dirty_files_detects_modified_files() {
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

        // Commit a file, then modify it.
        std::fs::write(root.join("data.bin"), b"original").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "data.bin"])
            .current_dir(root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        std::fs::write(root.join("data.bin"), b"modified").unwrap();

        let dirty = query_dirty_files(root);
        assert!(dirty.contains("data.bin"));
    }

    #[test]
    fn query_dirty_files_detects_untracked_files() {
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

        std::fs::write(root.join("new.bin"), b"untracked").unwrap();

        let dirty = query_dirty_files(root);
        assert!(dirty.contains("new.bin"));
    }

    #[test]
    fn query_dirty_files_excludes_hydrated_pointer_files() {
        // Simulate the hydrate cycle: commit a pointer blob, then
        // replace the worktree file with full content. The dirty check
        // must NOT report this as dirty — it's a hydrated file.
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

        // Write a valid crab pointer and commit it.
        let content = vec![0xAB; 4096];
        let hash = blake3::hash(&content);
        let pointer = crab_types::pointer::Pointer {
            file_hash: *hash.as_bytes(),
            size: 4096,
            shard_hint: None,
        };
        std::fs::write(root.join("data.bin"), pointer.serialize()).unwrap();
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

        // "Hydrate" the file — replace pointer with full content.
        std::fs::write(root.join("data.bin"), &content).unwrap();

        // The file is modified from git's perspective, but the index
        // blob is a pointer — so it should NOT be in the dirty set.
        let dirty = query_dirty_files(root);
        assert!(
            !dirty.contains("data.bin"),
            "hydrated file should not be reported as dirty, got: {dirty:?}"
        );
    }

    #[test]
    fn query_dirty_files_still_detects_truly_modified_files() {
        // A file whose index blob is NOT a pointer should still be dirty.
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

        // Commit a regular (non-pointer) file.
        std::fs::write(root.join("readme.txt"), b"original content").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "readme.txt"])
            .current_dir(root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        // Modify the file.
        std::fs::write(root.join("readme.txt"), b"modified content").unwrap();

        let dirty = query_dirty_files(root);
        assert!(
            dirty.contains("readme.txt"),
            "truly modified file should be dirty, got: {dirty:?}"
        );
    }

    #[cfg(feature = "gix-worktree")]
    #[test]
    fn query_dirty_files_detects_modified_files_in_linked_worktree() {
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

        std::fs::write(root.join("data.bin"), b"original").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "data.bin"])
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
        std::fs::write(linked.join("data.bin"), b"modified").unwrap();

        let dirty = query_dirty_files_via_gix(&linked).unwrap();
        assert!(
            dirty.contains("data.bin"),
            "linked worktree modified file should be dirty, got: {dirty:?}"
        );
    }

    #[test]
    fn dehydrate_refuses_hydrated_content_that_does_not_match_committed_pointer() {
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
        let original = vec![0xAB; 4096];
        let pointer = crab_types::pointer::Pointer {
            file_hash: *blake3::hash(&original).as_bytes(),
            size: original.len() as u64,
            shard_hint: None,
        };
        std::fs::write(root.join("data.bin"), pointer.serialize()).unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", ".gitattributes", "data.bin"])
            .current_dir(root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "add pointer"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let modified = vec![0xCD; 4096];
        std::fs::write(root.join("data.bin"), &modified).unwrap();

        let args = DehydrateArgs {
            all: true,
            ..default_args()
        };
        let cancel = CancellationToken::new();
        let error = run_dehydrate_in(root, &args, &cancel).unwrap_err();

        assert_eq!(std::fs::read(root.join("data.bin")).unwrap(), modified);
        assert!(error.to_string().contains("committed pointer references"));
    }

    #[test]
    fn dehydrate_refuses_tracked_content_without_committed_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let initialized = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if initialized.is_err() || !initialized.unwrap().success() {
            return;
        }

        for args in [
            ["config", "user.email", "test@test.com"],
            ["config", "user.name", "Test"],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap();
            assert!(status.success());
        }

        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        let content = vec![0xAB; 4096];
        std::fs::write(root.join("data.bin"), &content).unwrap();

        let attributes_added = std::process::Command::new("git")
            .args(["add", ".gitattributes"])
            .current_dir(root)
            .status()
            .unwrap();
        assert!(attributes_added.success());
        let raw_oid = std::process::Command::new("git")
            .args(["hash-object", "-w", "--no-filters", "data.bin"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(raw_oid.status.success());
        let raw_oid = String::from_utf8(raw_oid.stdout).unwrap();
        let cache_info = format!("100644,{},data.bin", raw_oid.trim());
        let index_updated = std::process::Command::new("git")
            .args(["update-index", "--add", "--cacheinfo", &cache_info])
            .current_dir(root)
            .status()
            .unwrap();
        assert!(index_updated.success());
        let committed = std::process::Command::new("git")
            .args(["commit", "-m", "raw tracked file"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(committed.success());

        let args = DehydrateArgs {
            all: true,
            ..default_args()
        };
        let error = run_dehydrate_in(root, &args, &CancellationToken::new()).unwrap_err();

        assert_eq!(std::fs::read(root.join("data.bin")).unwrap(), content);
        assert!(error.to_string().contains("committed Git blob"));
    }

    #[test]
    fn dehydrate_linked_worktree_skips_dirty_and_invalidates_own_cache() {
        use crate::cache::{HydratedPointerCache, hydrated_pointer};

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

        let clean_content = vec![0xAB; 4096];
        let clean_pointer = Pointer {
            file_hash: *blake3::hash(&clean_content).as_bytes(),
            size: clean_content.len() as u64,
            shard_hint: None,
        }
        .serialize();
        std::fs::write(root.join("clean.bin"), &clean_pointer).unwrap();
        std::fs::write(root.join("dirty.bin"), b"committed bytes").unwrap();

        let _ = std::process::Command::new("git")
            .args(["add", ".gitattributes", "clean.bin", "dirty.bin"])
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

        std::fs::write(linked.join("clean.bin"), &clean_content).unwrap();
        std::fs::write(linked.join("dirty.bin"), b"local edits").unwrap();

        let main_cache_path =
            hydrated_pointer::cache_path_for_worktree_root(&root).expect("main cache");
        let linked_cache_path =
            hydrated_pointer::cache_path_for_worktree_root(&linked).expect("linked cache");
        assert_ne!(main_cache_path, linked_cache_path);

        let main_entry = hydrated_pointer::entry_for_path(&root.join("clean.bin"), &clean_pointer)
            .expect("main cache entry");
        HydratedPointerCache::update_on_disk(
            &main_cache_path,
            [("clean.bin".to_owned(), main_entry)],
        )
        .expect("seed main cache");
        let linked_entry =
            hydrated_pointer::entry_for_path(&linked.join("clean.bin"), &clean_pointer)
                .expect("linked cache entry");
        HydratedPointerCache::update_on_disk(
            &linked_cache_path,
            [("clean.bin".to_owned(), linked_entry)],
        )
        .expect("seed linked cache");

        let args = DehydrateArgs {
            all: true,
            ..default_args()
        };
        let cancel = CancellationToken::new();
        run_dehydrate_in(&linked, &args, &cancel).unwrap();

        assert_eq!(
            std::fs::read(linked.join("clean.bin")).unwrap(),
            clean_pointer
        );
        assert_eq!(
            std::fs::read(linked.join("dirty.bin")).unwrap(),
            b"local edits"
        );
        assert!(
            HydratedPointerCache::load_sync(&linked_cache_path)
                .get("clean.bin")
                .is_none()
        );
        assert!(
            HydratedPointerCache::load_sync(&main_cache_path)
                .get("clean.bin")
                .is_some()
        );
    }

    // --- walk_hydrated_files tests ---

    #[test]
    fn walk_collects_hydrated_files_only() {
        let dir = setup_tracked_dir();

        // Hydrated file (non-pointer content).
        std::fs::write(dir.path().join("data.bin"), vec![0xAB; 8192]).unwrap();

        // Pointer file (should be skipped).
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("ptr.bin"), ptr.serialize()).unwrap();

        let filter = build_filter(&["**/*".to_owned()], &[]).unwrap();
        let tracked = TrackedClassifier::open(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let mut out = Vec::new();
        walk_hydrated_files(dir.path(), dir.path(), &tracked, &filter, &cancel, &mut out).unwrap();

        assert_eq!(out.len(), 1);
        assert!(out[0].to_string_lossy().contains("data.bin"));
    }

    #[test]
    fn walk_skips_hidden_directories() {
        let dir = setup_tracked_dir();
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("secret.bin"), vec![0xAB; 8192]).unwrap();

        let filter = build_filter(&["**/*".to_owned()], &[]).unwrap();
        let tracked = TrackedClassifier::open(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let mut out = Vec::new();
        walk_hydrated_files(dir.path(), dir.path(), &tracked, &filter, &cancel, &mut out).unwrap();

        assert!(out.is_empty(), "hidden dir files should be skipped");
    }

    #[test]
    fn walk_recurses_into_subdirectories() {
        let dir = setup_tracked_dir();
        let sub = dir.path().join("models");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("weights.bin"), vec![0xAB; 8192]).unwrap();

        let filter = build_filter(&["**/*".to_owned()], &[]).unwrap();
        let tracked = TrackedClassifier::open(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let mut out = Vec::new();
        walk_hydrated_files(dir.path(), dir.path(), &tracked, &filter, &cancel, &mut out).unwrap();

        assert_eq!(out.len(), 1);
        assert!(out[0].to_string_lossy().contains("weights.bin"));
    }

    #[test]
    fn walk_respects_cancellation() {
        let dir = setup_tracked_dir();
        std::fs::write(dir.path().join("data.bin"), vec![0xAB; 8192]).unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();

        let filter = build_filter(&["**/*".to_owned()], &[]).unwrap();
        let tracked = TrackedClassifier::open(dir.path()).unwrap();
        let mut out = Vec::new();
        let result =
            walk_hydrated_files(dir.path(), dir.path(), &tracked, &filter, &cancel, &mut out);
        assert!(result.is_err());
    }

    // --- run_dehydrate_in integration tests ---

    #[test]
    fn dehydrate_replaces_hydrated_files_with_pointers() {
        let dir = setup_tracked_dir();
        let content = vec![0xAB; 4096];
        std::fs::write(dir.path().join("model.bin"), &content).unwrap();

        let args = DehydrateArgs {
            patterns: vec!["*.bin".to_owned()],
            ..default_args()
        };
        let cancel = CancellationToken::new();
        run_dehydrate_in(dir.path(), &args, &cancel).unwrap();

        // File should now be a pointer.
        let path = dir.path().join("model.bin");
        assert!(is_working_tree_pointer(&path).unwrap());

        let written = std::fs::read(&path).unwrap();
        let ptr = Pointer::parse(&written).unwrap();
        assert_eq!(ptr.size, 4096);
    }

    #[test]
    fn dehydrate_skips_pointer_files() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);
        let pointer_bytes = ptr.serialize();
        std::fs::write(dir.path().join("already.bin"), &pointer_bytes).unwrap();

        let args = DehydrateArgs {
            all: true,
            ..default_args()
        };
        let cancel = CancellationToken::new();
        run_dehydrate_in(dir.path(), &args, &cancel).unwrap();

        // File should still be the same pointer.
        let written = std::fs::read(dir.path().join("already.bin")).unwrap();
        assert_eq!(written, pointer_bytes);
    }

    #[test]
    fn dehydrate_all_processes_all_hydrated_files() {
        let dir = setup_tracked_dir();
        std::fs::write(dir.path().join("a.bin"), vec![0xAA; 1024]).unwrap();
        std::fs::write(dir.path().join("b.bin"), vec![0xBB; 2048]).unwrap();

        let args = DehydrateArgs {
            all: true,
            ..default_args()
        };
        let cancel = CancellationToken::new();
        run_dehydrate_in(dir.path(), &args, &cancel).unwrap();

        assert!(is_working_tree_pointer(&dir.path().join("a.bin")).unwrap());
        assert!(is_working_tree_pointer(&dir.path().join("b.bin")).unwrap());
    }

    #[test]
    fn dehydrate_no_args_prints_help() {
        let dir = setup_tracked_dir();
        let args = default_args();
        let cancel = CancellationToken::new();
        run_dehydrate_in(dir.path(), &args, &cancel).unwrap();
    }

    #[test]
    fn dehydrate_no_gitattributes_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let args = DehydrateArgs {
            all: true,
            ..default_args()
        };
        let cancel = CancellationToken::new();
        run_dehydrate_in(dir.path(), &args, &cancel).unwrap();
    }

    #[test]
    fn dehydrate_non_matching_pattern_reports_nothing() {
        let dir = setup_tracked_dir();
        std::fs::write(dir.path().join("data.bin"), vec![0xAB; 8192]).unwrap();

        let args = DehydrateArgs {
            patterns: vec!["*.txt".to_owned()],
            ..default_args()
        };
        let cancel = CancellationToken::new();
        run_dehydrate_in(dir.path(), &args, &cancel).unwrap();

        // File should still be hydrated (not a pointer).
        assert!(!is_working_tree_pointer(&dir.path().join("data.bin")).unwrap());
    }

    #[test]
    fn atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("output.bin");
        let content = b"hello world";
        atomic_write(&dest, content).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("output.bin");
        std::fs::write(&dest, b"old content").unwrap();
        let content = b"new content";
        atomic_write(&dest, content).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_executable_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("executable.bin");
        std::fs::write(&dest, b"old content").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();

        atomic_write(&dest, b"new content").unwrap();

        assert_eq!(std::fs::metadata(&dest).unwrap().mode() & 0o777, 0o755);
    }

    // --- build_profile_protection tests ---

    fn setup_prefetch_dir(root: &Path, toml_content: &str) {
        let crab_dir = root.join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();
        std::fs::write(crab_dir.join("prefetch.toml"), toml_content).unwrap();
    }

    #[test]
    fn profile_protection_returns_none_when_ignore_profiles() {
        let dir = tempfile::tempdir().unwrap();
        setup_prefetch_dir(
            dir.path(),
            "version = 1\n\n[[profile]]\nname = \"always\"\npaths = [\"*.md\"]\n",
        );
        let result = build_profile_protection(dir.path(), true);
        assert!(result.is_none());
    }

    #[test]
    fn profile_protection_returns_none_when_no_prefetch_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = build_profile_protection(dir.path(), false);
        assert!(result.is_none());
    }

    #[test]
    fn profile_protection_returns_none_when_no_always_profile() {
        let dir = tempfile::tempdir().unwrap();
        setup_prefetch_dir(
            dir.path(),
            "version = 1\n\n[[profile]]\nname = \"ci\"\npaths = [\"tests/**\"]\n",
        );
        let result = build_profile_protection(dir.path(), false);
        assert!(result.is_none());
    }

    #[test]
    fn profile_protection_builds_glob_set_for_always_profile() {
        let dir = tempfile::tempdir().unwrap();
        setup_prefetch_dir(
            dir.path(),
            "version = 1\n\n[[profile]]\nname = \"always\"\npaths = [\"*.md\", \"src/**/*.rs\"]\n",
        );
        let glob_set = build_profile_protection(dir.path(), false).unwrap();
        assert!(glob_set.is_match("README.md"));
        assert!(glob_set.is_match("src/main.rs"));
        assert!(glob_set.is_match("src/cmd/dehydrate.rs"));
        assert!(!glob_set.is_match("data.bin"));
    }

    #[test]
    fn profile_protection_warns_on_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        setup_prefetch_dir(dir.path(), "this is not valid toml {{{}}}");
        let result = build_profile_protection(dir.path(), false);
        assert!(result.is_none());
    }

    #[test]
    fn profile_protection_returns_none_for_empty_always_paths() {
        let dir = tempfile::tempdir().unwrap();
        setup_prefetch_dir(
            dir.path(),
            "version = 1\n\n[[profile]]\nname = \"always\"\npaths = []\n",
        );
        let result = build_profile_protection(dir.path(), false);
        assert!(result.is_none());
    }

    // --- dehydrate with profile protection integration tests ---

    #[test]
    fn dehydrate_all_respects_always_profile() {
        let dir = setup_tracked_dir();
        setup_prefetch_dir(
            dir.path(),
            "version = 1\n\n[[profile]]\nname = \"always\"\npaths = [\"*.bin\"]\n",
        );
        std::fs::write(dir.path().join("model.bin"), vec![0xAB; 4096]).unwrap();

        let args = DehydrateArgs {
            all: true,
            ..default_args()
        };
        let cancel = CancellationToken::new();
        run_dehydrate_in(dir.path(), &args, &cancel).unwrap();

        // File should remain hydrated because it matches the always profile.
        assert!(!is_working_tree_pointer(&dir.path().join("model.bin")).unwrap());
    }

    #[test]
    fn dehydrate_all_with_ignore_profiles_overrides_protection() {
        let dir = setup_tracked_dir();
        setup_prefetch_dir(
            dir.path(),
            "version = 1\n\n[[profile]]\nname = \"always\"\npaths = [\"*.bin\"]\n",
        );
        std::fs::write(dir.path().join("model.bin"), vec![0xAB; 4096]).unwrap();

        let args = DehydrateArgs {
            all: true,
            ignore_profiles: true,
            ..default_args()
        };
        let cancel = CancellationToken::new();
        run_dehydrate_in(dir.path(), &args, &cancel).unwrap();

        // File should be dehydrated because --ignore-profiles was set.
        assert!(is_working_tree_pointer(&dir.path().join("model.bin")).unwrap());
    }

    #[test]
    fn dehydrate_protects_only_matching_files() {
        let dir = setup_tracked_dir();
        // Protect only *.md files, not *.bin.
        setup_prefetch_dir(
            dir.path(),
            "version = 1\n\n[[profile]]\nname = \"always\"\npaths = [\"*.md\"]\n",
        );
        std::fs::write(dir.path().join("data.bin"), vec![0xAB; 4096]).unwrap();

        let args = DehydrateArgs {
            all: true,
            ..default_args()
        };
        let cancel = CancellationToken::new();
        run_dehydrate_in(dir.path(), &args, &cancel).unwrap();

        // *.bin is not protected, so it should be dehydrated.
        assert!(is_working_tree_pointer(&dir.path().join("data.bin")).unwrap());
    }

    #[test]
    fn dehydrate_pattern_mode_respects_always_profile() {
        let dir = setup_tracked_dir();
        setup_prefetch_dir(
            dir.path(),
            "version = 1\n\n[[profile]]\nname = \"always\"\npaths = [\"*.bin\"]\n",
        );
        std::fs::write(dir.path().join("model.bin"), vec![0xAB; 4096]).unwrap();

        let args = DehydrateArgs {
            patterns: vec!["*.bin".to_owned()],
            ..default_args()
        };
        let cancel = CancellationToken::new();
        run_dehydrate_in(dir.path(), &args, &cancel).unwrap();

        // Protected even in pattern mode.
        assert!(!is_working_tree_pointer(&dir.path().join("model.bin")).unwrap());
    }
}
