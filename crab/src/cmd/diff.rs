//! `crab diff` — chunk-level diff between two git refs.
//!
//! Compares crab-tracked files using file-index and shard metadata, producing
//! per-file reports of changed chunks, affected bytes, and reuse ratio. The
//! optional format-aware annotations fetch only the bounded header/footer
//! chunks declared by their format hint.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use serde::Serialize;

use crate::cache::LocalCache;
use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::emit_json;
use crate::diff::format_hint::{ChunkRequest, FileVersion, detect_format_hint};
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
            emit_json("diff", "1.1", payload)?;
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
            emit_json("diff", "1.1", payload)?;
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

    // Stage 3: Resolve chunk sequences. Keep the read facade and router alive
    // for the optional format-aware annotation pass below; both share the
    // resolver's verified xorb cache and therefore do not duplicate metadata
    // reads.
    let mut annotation_context = None;
    let sequences = if hashes_to_resolve.is_empty() {
        HashMap::new()
    } else {
        let (store, prefix) = create_store_and_prefix(&config, &cancel).await?;
        let cache = std::sync::Arc::new(LocalCache::new(crate::cache::default_cache_root()));
        let router = crate::storage::StoreLayout::new(
            crate::storage::Store::from_storage(store.origin().clone()),
            prefix,
        );
        let resolver = TermResolver::new(
            store.clone(),
            router.clone(),
            cache,
            config.download_concurrency,
        )?;
        let sequences = resolver
            .resolve_sequences_batch(
                &hashes_to_resolve,
                ChunkSequenceSourceKind::Committed,
                &cancel,
            )
            .await?;
        annotation_context = Some((store, router));
        sequences
    };
    check_cancelled(&cancel)?;

    // Stage 4: Compare chunk sequences and build reports.
    let mut entries: Vec<FileDiffEntry> = Vec::new();
    for (path, status, old_ptr, new_ptr) in &pairs {
        check_cancelled(&cancel)?;

        let mut report = match status {
            FileStatus::Modified => {
                let old_hash = old_ptr.as_ref().map(|p| MerkleHash::from(p.file_hash));
                let new_hash = new_ptr.as_ref().map(|p| MerkleHash::from(p.file_hash));
                let old_sequence = old_hash.and_then(|h| sequences.get(&h));
                let new_sequence = new_hash.and_then(|h| sequences.get(&h));

                if let (Some(old_seq), Some(new_seq)) = (old_sequence, new_sequence) {
                    compare_sequences(path, old_seq, new_seq)
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
                    compare_sequences(path, &empty_old, new_seq)
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

        if !args.no_annotations {
            let old_sequence = old_ptr
                .as_ref()
                .and_then(|pointer| sequences.get(&MerkleHash::from(pointer.file_hash)));
            let new_sequence = new_ptr
                .as_ref()
                .and_then(|pointer| sequences.get(&MerkleHash::from(pointer.file_hash)));
            if let Some((store, router)) = annotation_context.as_ref() {
                apply_annotations(
                    &mut report,
                    old_sequence,
                    new_sequence,
                    store,
                    router,
                    &cancel,
                )
                .await?;
            }
        }

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
        emit_json("diff", "1.1", payload)?;

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
/// `FormatHint` protocol and bounded, verified xorb reads.
async fn apply_annotations(
    report: &mut ChunkDiffReport,
    old_sequence: Option<&ChunkSequence>,
    new_sequence: Option<&ChunkSequence>,
    store: &crab_cache_store::CachingStore,
    router: &crate::storage::StoreLayout,
    cancel: &CancellationToken,
) -> Result<()> {
    if report.changed_byte_ranges.is_empty() {
        return Ok(());
    }

    let Some(hint) = detect_format_hint(&report.path) else {
        return Ok(());
    };

    let file_size = report.new_size.max(report.old_size);
    let num_segments = new_sequence
        .or(old_sequence)
        .map_or(0, |sequence| sequence.spans.len());
    let requests = hint.required_chunks(file_size, num_segments);
    if requests.is_empty() {
        return Ok(());
    }

    let chunk_data =
        fetch_annotation_chunks(&requests, old_sequence, new_sequence, store, router, cancel)
            .await?;

    debug!(
        path = %report.path,
        format = hint.format_name(),
        chunks = chunk_data.iter().filter(|bytes| !bytes.is_empty()).count(),
        "format hint chunks fetched"
    );
    report.annotations = hint.annotate(&chunk_data, &report.changed_byte_ranges);
    Ok(())
}

/// Fetch the bounded chunks requested by a format hint.
///
/// Requests are grouped by xorb and coalesced before reading. A single xorb
/// therefore incurs one cache/origin read for all requested chunks, even when
/// both file versions use it. The non-installing reader keeps an annotation
/// probe from pinning a large xorb in the local cache. Missing origins are
/// represented by empty bytes so format parsers retain best-effort behavior.
async fn fetch_annotation_chunks(
    requests: &[ChunkRequest],
    old_sequence: Option<&ChunkSequence>,
    new_sequence: Option<&ChunkSequence>,
    store: &crab_cache_store::CachingStore,
    router: &crate::storage::StoreLayout,
    cancel: &CancellationToken,
) -> Result<Vec<Bytes>> {
    let mut selected = Vec::with_capacity(requests.len());
    let mut ranges_by_xorb: HashMap<MerkleHash, Vec<(u32, u32)>> = HashMap::new();

    for request in requests {
        check_cancelled(cancel)?;
        let sequence = match request.version {
            FileVersion::Old => old_sequence,
            FileVersion::New => new_sequence,
        };
        let Some(span) = sequence.and_then(|sequence| sequence.spans.get(request.segment_index))
        else {
            selected.push(None);
            continue;
        };
        let Some(xorb_hash) = span.origin.xorb_hash else {
            selected.push(None);
            continue;
        };
        let Some(xorb_chunk_index) = span.origin.xorb_chunk_index else {
            selected.push(None);
            continue;
        };
        let end = xorb_chunk_index
            .checked_add(1)
            .ok_or_else(|| CrabError::CorruptObject {
                path: format!("xorb:{}", xorb_hash.hex()),
                reason: "annotation chunk index overflows u32".to_owned(),
            })?;
        ranges_by_xorb
            .entry(xorb_hash)
            .or_default()
            .push((xorb_chunk_index, end));
        selected.push(Some((xorb_hash, xorb_chunk_index)));
    }

    let mut chunk_bytes: HashMap<(MerkleHash, u32), Bytes> = HashMap::new();
    for (xorb_hash, ranges) in ranges_by_xorb {
        check_cancelled(cancel)?;
        let ranges = coalesce_annotation_ranges(ranges);
        let (data, offsets) = store
            .get_xorb_chunks_without_install(&router.xorb_path(&xorb_hash), &xorb_hash, &ranges)
            .await
            .map_err(CrabError::from)?;
        let mut offset_index = 0usize;
        for (start, end) in ranges {
            for xorb_chunk_index in start..end {
                let begin =
                    offsets
                        .get(offset_index)
                        .copied()
                        .ok_or_else(|| CrabError::CorruptObject {
                            path: format!("xorb:{}", xorb_hash.hex()),
                            reason: "annotation response omitted chunk offset".to_owned(),
                        })?;
                let finish = offsets.get(offset_index + 1).copied().ok_or_else(|| {
                    CrabError::CorruptObject {
                        path: format!("xorb:{}", xorb_hash.hex()),
                        reason: "annotation response omitted chunk end offset".to_owned(),
                    }
                })?;
                let begin = usize::try_from(begin).map_err(|_| CrabError::CorruptObject {
                    path: format!("xorb:{}", xorb_hash.hex()),
                    reason: "annotation chunk offset overflows usize".to_owned(),
                })?;
                let finish = usize::try_from(finish).map_err(|_| CrabError::CorruptObject {
                    path: format!("xorb:{}", xorb_hash.hex()),
                    reason: "annotation chunk end offset overflows usize".to_owned(),
                })?;
                if begin > finish || finish > data.len() {
                    return Err(CrabError::CorruptObject {
                        path: format!("xorb:{}", xorb_hash.hex()),
                        reason: "annotation chunk offsets exceed response bytes".to_owned(),
                    });
                }
                chunk_bytes.insert((xorb_hash, xorb_chunk_index), data.slice(begin..finish));
                offset_index += 1;
            }
        }
        if offset_index + 1 != offsets.len() {
            return Err(CrabError::CorruptObject {
                path: format!("xorb:{}", xorb_hash.hex()),
                reason: "annotation response returned unexpected chunk offsets".to_owned(),
            });
        }
    }

    Ok(selected
        .into_iter()
        .map(|selected| {
            selected
                .and_then(|key| chunk_bytes.get(&key).cloned())
                .unwrap_or_default()
        })
        .collect())
}

fn coalesce_annotation_ranges(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    ranges.sort_unstable();
    ranges.dedup();
    let mut coalesced = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some((_, previous_end)) = coalesced.last_mut()
            && start <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
        } else {
            coalesced.push((start, end));
        }
    }
    coalesced
}

#[cfg(test)]
mod tests {
    use super::coalesce_annotation_ranges;

    #[test]
    fn coalesce_annotation_ranges_deduplicates_and_merges_adjacent_chunks() {
        assert_eq!(
            coalesce_annotation_ranges(vec![(4, 5), (1, 2), (2, 3), (4, 5), (8, 9)]),
            vec![(1, 3), (4, 5), (8, 9)]
        );
    }
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
