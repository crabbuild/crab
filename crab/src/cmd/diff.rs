//! `crab diff` — chunk-level diff between two git refs.
//!
//! Compares crab-tracked files using only metadata (file-index + shards),
//! producing per-file reports of which chunks changed, bytes affected,
//! and reuse ratio — with zero data transfer.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use serde::Serialize;

use crate::cache::LocalCache;
use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::emit_json;
use crate::diff::format_hint::detect_format_hint;
use crate::diff::formatter::format_diff;
use crate::diff::term_resolver::TermResolver;
use crab_diff::chunk_sequence::{ChunkSequence, compare_sequences};
use crab_diff::pair_files;
use crab_diff::types::{
    ChunkDiffReport, ChunkSequenceSourceKind, DiffSummary, FileDiffEntry, FileStatus, OutputMode,
};
use crab_git::resolve_pointer_ref;
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;

/// Envelope payload for `crab diff --json`.
///
/// Wraps the existing `files` + `summary` shape so the inner payload
/// remains byte-compatible with the pre-envelope format.
#[derive(Debug, Clone, Serialize)]
struct DiffPayload<'a> {
    files: Vec<&'a FileDiffEntry>,
    summary: &'a DiffSummary,
}

/// Arguments for `crab diff`.
#[derive(Debug, Clone)]
pub struct DiffArgs {
    pub ref1: String,
    pub ref2: Option<String>,
    pub paths: Vec<String>,
    pub mode: crate::core::output::OutputMode,
    pub stat: bool,
    pub name_only: bool,
    pub verbose: bool,
    pub byte_ranges: bool,
    pub no_color: bool,
    pub no_annotations: bool,
}

/// Entry point for `crab diff`.
pub async fn run_diff(args: DiffArgs, config: Config, cancel: CancellationToken) -> Result<()> {
    let _span = tracing::info_span!("diff", ref1 = %args.ref1).entered();

    // Discover the git directory.
    let git_dir = discover_git_dir()?;

    // Support `ref1..ref2` range syntax (git convention) in addition to
    // separate positional arguments. When ref1 contains ".." and ref2 is
    // None, split on the first ".." to extract both sides.
    let (effective_ref1, effective_ref2);
    if args.ref2.is_none() {
        if let Some((left, right)) = args.ref1.split_once("..") {
            effective_ref1 = if left.is_empty() { "HEAD" } else { left };
            effective_ref2 = if right.is_empty() { "HEAD" } else { right };
        } else {
            effective_ref1 = args.ref1.as_str();
            effective_ref2 = "HEAD";
        }
    } else {
        effective_ref1 = args.ref1.as_str();
        effective_ref2 = args.ref2.as_deref().unwrap_or("HEAD");
    }

    // Path filter (empty vec means no filter).
    let path_filter: Option<Vec<String>> = if args.paths.is_empty() {
        None
    } else {
        Some(args.paths.clone())
    };
    let filter_ref = path_filter.as_deref();

    // Stage 1: Resolve both refs to pointer maps.
    let old_map = match resolve_pointer_ref(&git_dir, effective_ref1, filter_ref) {
        Ok(m) => m,
        Err(crab_git::pointer_ref::PointerRefError::NotFound { refspec }) => {
            let path = refspec;
            eprintln!("error: unknown ref '{path}'");
            return Err(CrabError::NotFound { path });
        }
        Err(e) => return Err(CrabError::from(e)),
    };
    check_cancelled(&cancel)?;

    let new_map = match resolve_pointer_ref(&git_dir, effective_ref2, filter_ref) {
        Ok(m) => m,
        Err(crab_git::pointer_ref::PointerRefError::NotFound { refspec }) => {
            let path = refspec;
            eprintln!("error: unknown ref '{path}'");
            return Err(CrabError::NotFound { path });
        }
        Err(e) => return Err(CrabError::from(e)),
    };
    check_cancelled(&cancel)?;

    // Check for no tracked files.
    if old_map.is_empty() && new_map.is_empty() {
        if args.mode == crate::core::output::OutputMode::Json {
            let payload = DiffPayload {
                files: Vec::new(),
                summary: &DiffSummary::default(),
            };
            emit_json("diff", "1.1", payload);
        } else {
            println!("no crab-tracked files found");
        }
        return Ok(());
    }

    // Stage 2: Pair files by path.
    let pairs = pair_files(&old_map, &new_map);

    if pairs.is_empty() {
        if args.mode == crate::core::output::OutputMode::Json {
            let payload = DiffPayload {
                files: Vec::new(),
                summary: &DiffSummary::default(),
            };
            emit_json("diff", "1.1", payload);
        } else {
            let total_tracked = old_map.len().max(new_map.len());
            println!(
                "no changes to crab-tracked files ({total_tracked} tracked file{} unchanged)",
                if total_tracked == 1 { "" } else { "s" }
            );
        }
        return Ok(());
    }

    // Collect file hashes that need chunk-sequence resolution.
    let mut hashes_to_resolve: Vec<(MerkleHash, Option<MerkleHash>, u64)> = Vec::new();
    for (_path, status, old_ptr, new_ptr) in &pairs {
        match status {
            FileStatus::Modified => {
                if let Some(ptr) = old_ptr {
                    hashes_to_resolve.push(hash_with_hint_and_size(ptr));
                }
                if let Some(ptr) = new_ptr {
                    hashes_to_resolve.push(hash_with_hint_and_size(ptr));
                }
            }
            FileStatus::Added => {
                if let Some(ptr) = new_ptr {
                    hashes_to_resolve.push(hash_with_hint_and_size(ptr));
                }
            }
            FileStatus::Deleted => {
                if let Some(ptr) = old_ptr {
                    hashes_to_resolve.push(hash_with_hint_and_size(ptr));
                }
            }
            FileStatus::GitNative => {}
        }
    }

    // Stage 3: Resolve chunk sequences.
    let sequences = if hashes_to_resolve.is_empty() {
        HashMap::new()
    } else {
        let (store, prefix) = create_store_and_prefix(&config, &cancel).await?;
        let cache = std::sync::Arc::new(LocalCache::new(crate::cache::default_cache_root()));
        let router = crate::storage::StoreLayout::new(
            crate::storage::Store::from_storage(store.origin().clone()),
            prefix,
        );
        let resolver = TermResolver::new(store, router, cache, config.download_concurrency);
        resolver
            .resolve_sequences_batch(
                &hashes_to_resolve,
                ChunkSequenceSourceKind::Committed,
                &cancel,
            )
            .await?
    };
    check_cancelled(&cancel)?;

    // Stage 4: Compare chunk sequences and build reports.
    let mut entries: Vec<FileDiffEntry> = Vec::new();
    for (path, status, old_ptr, new_ptr) in &pairs {
        check_cancelled(&cancel)?;

        let report = match status {
            FileStatus::Modified => {
                let old_hash = old_ptr.as_ref().map(|p| MerkleHash::from(p.file_hash));
                let new_hash = new_ptr.as_ref().map(|p| MerkleHash::from(p.file_hash));
                let old_sequence = old_hash.and_then(|h| sequences.get(&h));
                let new_sequence = new_hash.and_then(|h| sequences.get(&h));

                if let (Some(old_seq), Some(new_seq)) = (old_sequence, new_sequence) {
                    let mut report = compare_sequences(path, old_seq, new_seq);

                    // Apply format hints if annotations are enabled.
                    if !args.no_annotations {
                        apply_annotations(&mut report);
                    }
                    report
                } else {
                    // Graceful degradation: metadata unavailable.
                    warn!(path = %path, "chunk-level diff unavailable, reporting as git-native");
                    make_git_native_report(path, old_ptr.as_ref(), new_ptr.as_ref())
                }
            }
            FileStatus::Added => {
                let new_hash = new_ptr.as_ref().map(|p| MerkleHash::from(p.file_hash));
                let new_sequence = new_hash.and_then(|h| sequences.get(&h));

                if let Some(new_seq) = new_sequence {
                    let empty_old = empty_sequence(ChunkSequenceSourceKind::Committed);
                    let mut report = compare_sequences(path, &empty_old, new_seq);
                    if !args.no_annotations {
                        apply_annotations(&mut report);
                    }
                    report
                } else {
                    warn!(path = %path, "chunk-level diff unavailable for added file");
                    make_git_native_report(path, old_ptr.as_ref(), new_ptr.as_ref())
                }
            }
            FileStatus::Deleted => {
                let old_hash = old_ptr.as_ref().map(|p| MerkleHash::from(p.file_hash));
                let old_sequence = old_hash.and_then(|h| sequences.get(&h));

                if let Some(old_seq) = old_sequence {
                    let empty_new = empty_sequence(ChunkSequenceSourceKind::Committed);
                    compare_sequences(path, old_seq, &empty_new)
                } else {
                    warn!(path = %path, "chunk-level diff unavailable for deleted file");
                    make_git_native_report(path, old_ptr.as_ref(), new_ptr.as_ref())
                }
            }
            FileStatus::GitNative => {
                make_git_native_report(path, old_ptr.as_ref(), new_ptr.as_ref())
            }
        };

        entries.push(FileDiffEntry { report });
    }

    // Build summary.
    let summary = build_summary(&entries);

    // JSON mode: wrap in envelope and emit directly, bypassing the formatter.
    if args.mode == crate::core::output::OutputMode::Json {
        let mut sorted: Vec<&FileDiffEntry> = entries.iter().collect();
        sorted.sort_by(|a, b| a.report.path.cmp(&b.report.path));
        let payload = DiffPayload {
            files: sorted,
            summary: &summary,
        };
        emit_json("diff", "1.1", payload);

        info!(
            files_changed = summary.files_changed,
            delta_bytes = summary.total_delta_bytes,
            "diff complete"
        );
        return Ok(());
    }

    // Determine text output mode.
    let mode = if args.stat {
        OutputMode::Stat
    } else if args.name_only {
        OutputMode::NameOnly
    } else if args.verbose {
        OutputMode::HumanVerbose
    } else {
        OutputMode::Human
    };

    // Determine color output.
    let color = !args.no_color && std::io::stdout().is_terminal();

    let mut stdout = std::io::stdout().lock();
    format_diff(
        &entries,
        &summary,
        mode,
        color,
        args.byte_ranges,
        &mut stdout,
    )?;

    info!(
        files_changed = summary.files_changed,
        delta_bytes = summary.total_delta_bytes,
        "diff complete"
    );

    Ok(())
}

/// Extract `(MerkleHash, Option<MerkleHash>, size)` from a pointer for sequence resolution.
fn hash_with_hint_and_size(ptr: &Pointer) -> (MerkleHash, Option<MerkleHash>, u64) {
    let file_hash = MerkleHash::from(ptr.file_hash);
    let shard_hint = ptr.shard_hint.map(MerkleHash::from);
    (file_hash, shard_hint, ptr.size)
}

fn empty_sequence(source: ChunkSequenceSourceKind) -> ChunkSequence {
    ChunkSequence {
        source,
        file_hash: MerkleHash::default(),
        file_size: 0,
        spans: Vec::new(),
    }
}

/// Build a `DiffSummary` from the list of file diff entries.
fn build_summary(entries: &[FileDiffEntry]) -> DiffSummary {
    let mut files_changed: u32 = 0;
    let mut total_segments_changed: u32 = 0;
    let mut total_delta_bytes: u64 = 0;

    for entry in entries {
        let r = &entry.report;
        if r.status != FileStatus::GitNative {
            files_changed += 1;
            total_segments_changed += r.added_segments + r.removed_segments;
            total_delta_bytes += r.delta_bytes;
        }
    }

    DiffSummary {
        files_changed,
        total_segments_changed,
        total_delta_bytes,
    }
}

/// Create a fallback report when chunk-level diff is unavailable.
///
/// Preserves the file status from the pairing phase and reports sizes
/// from the pointers. Used when chunk-sequence resolution fails.
fn make_git_native_report(
    path: &str,
    old_ptr: Option<&Pointer>,
    new_ptr: Option<&Pointer>,
) -> ChunkDiffReport {
    // Determine the correct status from pointer presence rather than
    // always using GitNative. This prevents "deleted" rendering for
    // files that are actually modified but have unresolvable metadata.
    let status = match (old_ptr, new_ptr) {
        (Some(_), Some(_)) => FileStatus::Modified,
        (None, Some(_)) => FileStatus::Added,
        (Some(_), None) => FileStatus::Deleted,
        (None, None) => FileStatus::GitNative,
    };
    let old_size = old_ptr.map_or(0, |p| p.size);
    let new_size = new_ptr.map_or(0, |p| p.size);
    let delta_bytes = new_size.abs_diff(old_size);
    ChunkDiffReport {
        path: path.to_owned(),
        status,
        old_size,
        new_size,
        unchanged_segments: 0,
        unchanged_bytes: 0,
        removed_segments: 0,
        removed_bytes: 0,
        added_segments: 0,
        added_bytes: 0,
        delta_bytes,
        dedup_ratio: 0.0,
        changed_byte_ranges: Vec::new(),
        segment_details: Vec::new(),
        annotations: Vec::new(),
        chunk_metrics: None,
    }
}

/// Apply format-aware annotations to a diff report using the two-phase
/// FormatHint protocol. Only the metadata-based annotation path is used
/// here (no chunk downloads for the MVP — annotations use canonical
/// byte ranges to produce byte-range-based annotations).
fn apply_annotations(report: &mut ChunkDiffReport) {
    if report.changed_byte_ranges.is_empty() {
        return;
    }

    let Some(hint) = detect_format_hint(&report.path) else {
        return;
    };

    debug!(
        path = %report.path,
        format = hint.format_name(),
        "format hint detected (chunk download not yet wired)"
    );

    // Full two-phase annotation requires downloading header/footer chunks
    // from the store. For now, annotations are left empty — the format
    // hint infrastructure is wired and ready for when chunk download is
    // integrated in a follow-up task.
}

/// Discover the `.git` directory from the current working directory.
///
/// Delegates to [`crate::git::discover::discover_git_dir`] which
/// wraps `gix_discover::upwards`. No subprocess required — gitoxide
/// handles GIT_DIR, bare repos, and worktrees natively.
fn discover_git_dir() -> Result<PathBuf> {
    crate::git::discover::discover_git_dir()
}

/// Read the remote URL and create a Store + prefix for metadata access.
pub async fn create_store_and_prefix(
    config: &Config,
    cancel: &CancellationToken,
) -> Result<(crab_cache_store::CachingStore, String)> {
    let cwd = std::env::current_dir()?;
    let url = crate::core::project_config::ProjectConfig::remote_url(&cwd)?;
    let parsed = crate::git::url::CrabUrl::parse(&url)?;

    let selection = crate::replication::select_read_store(config, &parsed, "diff", cancel).await?;
    let prefix = selection.router.repo_prefix().to_owned();
    let caching_store = crab_cache_store::CachingStore::new(selection.store, &config.cache)?;
    Ok((caching_store, prefix))
}
