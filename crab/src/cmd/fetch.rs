//! `crab fetch` — prewarm canonical local caches for selected Crab files.
//!
//! Selection comes from Git pointer blobs, not bucket-global object listing.
//! Each selected file is reconstructed into a hash-verifying sink through the
//! same shard, file-index, replica, and xet-core range-cache path as hydrate.

use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::cmd::hydrate::{HydrateArgs, ShardHydrator};
use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::event_payloads::{PERF_PHASE_SCHEMA, PerfPhasePayload};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::core::perf_phase::PhaseTimer;

const MAX_FETCH_CANDIDATES: usize = 1_000_000;

/// Arguments for the `crab fetch` command.
pub struct FetchArgs {
    /// Glob patterns to limit which files' objects are fetched.
    pub include: Vec<String>,
    /// Glob patterns to exclude from fetching.
    pub exclude: Vec<String>,
    /// Fetch all objects for all refs, not just HEAD.
    pub all: bool,
    /// Report what would be fetched without downloading.
    pub dry_run: bool,
    /// Skip the post-fetch shard-sync step that warms the local chunk index.
    pub no_sync_chunk_index: bool,
    /// Output mode resolved from `--json` / `--jsonl` flags.
    pub mode: OutputMode,
}

/// Summary of a completed fetch operation.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FetchSummary {
    /// Number of objects fetched.
    pub objects_fetched: u64,
    /// Total bytes downloaded.
    pub bytes_downloaded: u64,
    /// Number of objects skipped (already cached).
    pub objects_skipped: u64,
    /// Wall-clock duration of the operation in milliseconds.
    pub duration_ms: u64,
}

/// Run `crab fetch` in the current working directory.
pub async fn run_fetch(args: &FetchArgs, cancel: &CancellationToken) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    let worktree = crate::git::worktree::WorktreeContext::resolve_from_path(&cwd)?;
    run_fetch_in(&worktree.current_worktree_root, args, cancel).await
}

fn emit_phase(stream: Option<&Mutex<JsonlStream<Stdout>>>, payload: PerfPhasePayload) {
    if let Some(stream) = stream
        && let Ok(mut stream) = stream.lock()
    {
        stream.emit_schema_event(PERF_PHASE_SCHEMA, "event", payload);
    }
}

/// Fetch selected repository content through the canonical hydrate path.
pub async fn run_fetch_in(root: &Path, args: &FetchArgs, cancel: &CancellationToken) -> Result<()> {
    let config = Config::resolve_for_repo(root)?;
    let candidates = resolve_candidates(root, args, &config, cancel)?;
    if candidates.len() > MAX_FETCH_CANDIDATES {
        return Err(CrabError::Configuration {
            key: "fetch candidate count".to_owned(),
            origin: format!(
                "fetch selection exceeds the safety limit of {MAX_FETCH_CANDIDATES} files"
            ),
        });
    }
    let logical_bytes = candidates
        .iter()
        .try_fold(0u64, |bytes, (_, pointer)| bytes.checked_add(pointer.size))
        .ok_or_else(|| CrabError::Internal("selected fetch size exceeds u64".to_owned()))?;
    let selected = u64::try_from(candidates.len())
        .map_err(|_| CrabError::Internal("selected fetch count exceeds u64".to_owned()))?;
    let start = Instant::now();

    if args.dry_run {
        return emit_summary(
            args.mode,
            FetchSummary {
                objects_fetched: 0,
                bytes_downloaded: 0,
                objects_skipped: selected,
                duration_ms: elapsed_millis(start),
            },
            Some((selected, logical_bytes)),
            None,
        );
    }
    if candidates.is_empty() {
        return emit_summary(
            args.mode,
            FetchSummary {
                objects_fetched: 0,
                bytes_downloaded: 0,
                objects_skipped: 0,
                duration_ms: elapsed_millis(start),
            },
            None,
            None,
        );
    }

    check_cancelled(cancel)?;
    let parsed = read_remote(root)?;
    let selection =
        crate::replication::select_read_store(&config, &parsed, "fetch", cancel).await?;
    if let crate::replication::ReadSource::Replica { name } = &selection.source {
        tracing::debug!(replica = %name, "selected read replica for fetch");
    }
    let read_router = selection.router;
    let caching_store = crab_cache_store::CachingStore::new(selection.store, &config.cache)?;
    let mut hydrator = ShardHydrator::with_config_from_cli_layout(
        caching_store.clone(),
        read_router.clone(),
        &config,
    )?;
    let chunk_cache = crate::cache::xet_chunk_cache_from_config(&config)?;
    hydrator = hydrator.with_xet_chunk_cache(chunk_cache.cache);

    let jsonl_stream = (args.mode == OutputMode::Jsonl)
        .then(|| Mutex::new(JsonlStream::new("fetch.event", "1.0", std::io::stdout())));
    let phase = PhaseTimer::start("fetch", "hydration_prefetch");
    let prefetched = hydrator.prefetch_batch(&candidates, cancel).await?;
    emit_phase(
        jsonl_stream.as_ref(),
        phase.finish(0, prefetched.bytes_prefetched, prefetched.prefetched),
    );
    if prefetched.failed > 0 {
        return Err(CrabError::Protocol(format!(
            "fetch failed to prewarm {} of {} selected file(s)",
            prefetched.failed, selected
        )));
    }

    if !args.no_sync_chunk_index {
        check_cancelled(cancel)?;
        let phase = PhaseTimer::start("fetch", "shard_sync");
        let router = crate::storage::StoreLayout::new(
            crate::storage::Store::from_storage(caching_store.origin().clone()),
            read_router.repo_prefix().to_owned(),
        );
        let repo_hash = crate::git::push::compute_repo_hash(&parsed.repo_path);
        crate::metadata::shard_sync::run_post_fetch_shard_sync(
            router,
            &repo_hash,
            &crate::cache::default_cache_root(),
            None,
            args.mode == OutputMode::Text,
        )
        .await?;
        // The sync updates the persistent chunk index and local shard cache;
        // let that derived-state mutation settle before honoring cancellation.
        check_cancelled(cancel)?;
        emit_phase(jsonl_stream.as_ref(), phase.finish(0, 0, 1));
    }

    emit_summary(
        args.mode,
        FetchSummary {
            objects_fetched: prefetched.prefetched,
            bytes_downloaded: prefetched.bytes_prefetched,
            objects_skipped: 0,
            duration_ms: elapsed_millis(start),
        },
        None,
        jsonl_stream.as_ref(),
    )
}

fn resolve_candidates(
    root: &Path,
    args: &FetchArgs,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<Vec<(PathBuf, crab_types::pointer::Pointer)>> {
    if args.all {
        return crate::cmd::hydrate::resolve_all_ref_pointer_prefetch_candidates(
            root,
            &args.include,
            &args.exclude,
            cancel,
        );
    }
    let hydrate_args = HydrateArgs {
        patterns: Vec::new(),
        include: args.include.clone(),
        exclude: args.exclude.clone(),
        all: args.include.is_empty() && args.exclude.is_empty(),
        mode: OutputMode::Text,
        manifest: None,
        manifest_ref: None,
        profile: None,
        ignore_sparse: true,
        recover_from: None,
    };
    crate::cmd::hydrate::resolve_git_pointer_prefetch_candidates(
        root,
        &hydrate_args,
        config,
        cancel,
    )
}

fn read_remote(root: &Path) -> Result<crate::git::url::CrabUrl> {
    let path = root.join(".crab/remote");
    let url = std::fs::read_to_string(&path).map_err(|_| CrabError::Configuration {
        key: "no remote configured".into(),
        origin: path.display().to_string(),
    })?;
    crate::git::url::CrabUrl::parse(url.trim())
}

fn elapsed_millis(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn emit_summary(
    mode: OutputMode,
    summary: FetchSummary,
    dry_run_selection: Option<(u64, u64)>,
    jsonl_stream: Option<&Mutex<JsonlStream<Stdout>>>,
) -> Result<()> {
    match mode {
        OutputMode::Text => match dry_run_selection {
            Some((files, bytes)) => {
                eprintln!("fetch (dry run): would prewarm {files} file(s), {bytes} logical bytes")
            }
            None => eprintln!(
                "fetch complete: {} file(s), {} logical bytes verified",
                summary.objects_fetched, summary.bytes_downloaded
            ),
        },
        OutputMode::Json => emit_json("fetch", "1.0", &summary),
        OutputMode::Jsonl => {
            if let Some(stream) = jsonl_stream {
                stream
                    .lock()
                    .map_err(|_| CrabError::Internal("fetch output lock poisoned".to_owned()))?
                    .emit_result(&summary);
            } else {
                JsonlStream::new("fetch.event", "1.0", std::io::stdout()).emit_result(&summary);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_summary_does_not_claim_downloads() {
        let summary = FetchSummary {
            objects_fetched: 0,
            bytes_downloaded: 0,
            objects_skipped: 3,
            duration_ms: 1,
        };
        assert_eq!(summary.objects_skipped, 3);
        assert_eq!(summary.bytes_downloaded, 0);
    }
}
