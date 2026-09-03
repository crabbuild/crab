//! `crab prune` — evict current local cache objects to configured budgets.
//!
//! Prune is local-only. It never deletes remote storage; it evicts cached
//! chunks, xorbs, and shards that can be fetched again when needed.

use std::io::Stdout;

use serde::Serialize;
use tokio::pin;
use tokio_util::sync::CancellationToken;

use crate::cache::{
    LocalCache, PruneObjectKind, PruneOptions, PruneStats, PrunedCacheObject,
    prune_xet_chunk_cache_with_cancel,
};
use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::event_payloads::FileDonePayload;
use crate::core::output::{JsonlStream, OutputMode};

/// Arguments for the `crab prune` command.
pub struct PruneArgs {
    /// Report what would be pruned without deleting anything.
    pub dry_run: bool,
    /// Print each object as it is pruned.
    pub verbose: bool,
    /// Output mode resolved from `--json` / `--jsonl` flags.
    pub mode: OutputMode,
}

/// Terminal result payload for `--json` / `--jsonl` structured output.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PruneSummary {
    /// Number of objects pruned (or would-be-pruned in dry-run).
    pub objects_pruned: u64,
    /// Number of chunk cache objects pruned (or would-be-pruned).
    pub chunks_pruned: u64,
    /// Number of shard cache objects pruned (or would-be-pruned).
    pub shards_pruned: u64,
    /// Number of xorb cache objects pruned (or would-be-pruned).
    pub xorbs_pruned: u64,
    /// Total bytes freed (or would-be-freed in dry-run).
    pub bytes_freed: u64,
    /// Whether this was a dry-run (no mutations).
    pub dry_run: bool,
}

/// Run `crab prune`.
pub async fn run_prune(
    args: &PruneArgs,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
) -> Result<PruneSummary> {
    run_prune_with_cancel(args, jsonl_stream, &CancellationToken::new()).await
}

/// Run cache pruning while honoring cancellation during both cache families.
pub async fn run_prune_with_cancel(
    args: &PruneArgs,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
    cancel: &CancellationToken,
) -> Result<PruneSummary> {
    check_cancelled(cancel)?;
    let root = crate::cache::default_cache_root();
    super::cache::validate_destructive_cache_root(&root)?;
    let config = Config::resolve_local()?;
    let cache = LocalCache::with_limits(root, config.cache.max_bytes, Some(config.cache.max_bytes));
    let local_prune = cache.prune_with_options(PruneOptions {
        dry_run: args.dry_run,
        record_entries: args.verbose || args.mode == OutputMode::Jsonl,
    });
    pin!(local_prune);
    let mut stats = tokio::select! {
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        result = &mut local_prune => result?,
    };
    check_cancelled(cancel)?;
    let xet = prune_xet_chunk_cache_with_cancel(
        &config.effective_chunk_cache_dir(),
        config.cache.max_bytes,
        args.dry_run,
        args.verbose || args.mode == OutputMode::Jsonl,
        cancel,
    )
    .await?;
    check_cancelled(cancel)?;
    stats.chunks_evicted = stats.chunks_evicted.saturating_add(xet.entries_evicted);
    stats.bytes_freed = stats.bytes_freed.saturating_add(xet.bytes_freed);
    stats.entries.extend(
        xet.entries
            .into_iter()
            .map(|(path, bytes)| PrunedCacheObject {
                kind: PruneObjectKind::Chunk,
                bytes,
                path,
            }),
    );
    check_cancelled(cancel)?;
    finish_prune(&stats, args, jsonl_stream)
}

#[cfg(test)]
async fn run_prune_with_cache(
    cache: &LocalCache,
    args: &PruneArgs,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
) -> Result<PruneSummary> {
    let stats = cache
        .prune_with_options(PruneOptions {
            dry_run: args.dry_run,
            record_entries: args.verbose || args.mode == OutputMode::Jsonl,
        })
        .await?;
    finish_prune(&stats, args, jsonl_stream)
}

fn finish_prune(
    stats: &PruneStats,
    args: &PruneArgs,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
) -> Result<PruneSummary> {
    emit_pruned_entries(stats, args, jsonl_stream);
    let summary = PruneSummary::from_stats(&stats, args.dry_run);
    emit_text_summary(&summary, args);
    Ok(summary)
}

impl PruneSummary {
    fn from_stats(stats: &PruneStats, dry_run: bool) -> Self {
        Self {
            objects_pruned: stats.objects_evicted(),
            chunks_pruned: stats.chunks_evicted,
            shards_pruned: stats.shards_evicted,
            xorbs_pruned: stats.xorbs_evicted,
            bytes_freed: stats.bytes_freed,
            dry_run,
        }
    }
}

fn emit_pruned_entries(
    stats: &PruneStats,
    args: &PruneArgs,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
) {
    let action = if args.dry_run {
        "would prune"
    } else {
        "pruning"
    };
    let status = if args.dry_run { "skipped" } else { "ok" };

    for entry in &stats.entries {
        if args.verbose && !args.mode.is_machine() {
            eprintln!(
                "{action} {}: {} ({} bytes)",
                entry.kind.as_str(),
                entry.path.display(),
                entry.bytes
            );
        }
        if let Some(stream) = jsonl_stream
            && let Ok(mut s) = stream.lock()
        {
            s.emit_file_done(FileDonePayload {
                path: entry.path.display().to_string(),
                bytes: entry.bytes,
                duration_ms: 0,
                status: status.to_owned(),
            });
        }
    }
}

fn emit_text_summary(summary: &PruneSummary, args: &PruneArgs) {
    if args.mode.is_machine() {
        return;
    }

    let detail = format!(
        "chunks: {}, xorbs: {}, shards: {}",
        summary.chunks_pruned, summary.xorbs_pruned, summary.shards_pruned
    );
    if args.dry_run {
        eprintln!(
            "prune (dry run): would remove {} objects ({} bytes; {detail})",
            summary.objects_pruned, summary.bytes_freed
        );
    } else {
        eprintln!(
            "prune complete: removed {} objects ({} bytes freed; {detail})",
            summary.objects_pruned, summary.bytes_freed
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheKey;
    use crab_xet::hash::compute_data_hash;

    #[tokio::test]
    async fn prune_missing_cache_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(dir.path().join("missing"));
        let args = PruneArgs {
            dry_run: true,
            verbose: false,
            mode: OutputMode::Text,
        };

        let summary = run_prune_with_cache(&cache, &args, None).await.unwrap();

        assert_eq!(summary.objects_pruned, 0);
        assert_eq!(summary.bytes_freed, 0);
        assert!(summary.dry_run);
    }

    #[tokio::test]
    async fn prune_honors_cancellation_before_cache_resolution() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let args = PruneArgs {
            dry_run: true,
            verbose: false,
            mode: OutputMode::Text,
        };

        let error = run_prune_with_cancel(&args, None, &cancel)
            .await
            .unwrap_err();
        assert!(matches!(error, CrabError::Cancelled));
    }

    #[tokio::test]
    async fn dry_run_reports_current_cache_without_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(dir.path().join("cache"));
        seed_chunks(&cache).await;
        seed_shards(&cache).await;
        // Model a budget reduction after valid writes; write-time admission
        // would already evict entries if seeded with the final small budget.
        let cache = LocalCache::with_limits(cache.root().to_path_buf(), 100, Some(50));
        let args = PruneArgs {
            dry_run: true,
            verbose: false,
            mode: OutputMode::Json,
        };

        let summary = run_prune_with_cache(&cache, &args, None).await.unwrap();
        let stats = cache.stats().await.unwrap();

        assert!(summary.objects_pruned > 0);
        assert!(summary.bytes_freed > 0);
        assert!(stats.chunk_bytes > 100);
        assert!(stats.shard_bytes > 50);
    }

    #[tokio::test]
    async fn prune_evicts_current_cache_to_budget() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(dir.path().join("cache"));
        seed_chunks(&cache).await;
        seed_shards(&cache).await;
        let cache = LocalCache::with_limits(cache.root().to_path_buf(), 100, Some(50));
        let args = PruneArgs {
            dry_run: false,
            verbose: false,
            mode: OutputMode::Json,
        };

        let summary = run_prune_with_cache(&cache, &args, None).await.unwrap();
        let stats = cache.stats().await.unwrap();

        assert!(summary.chunks_pruned > 0);
        assert!(summary.shards_pruned > 0);
        assert!(stats.chunk_bytes <= 100);
        assert!(stats.shard_bytes <= 50);
    }

    async fn seed_chunks(cache: &LocalCache) {
        for i in 0u8..5 {
            let data = vec![i; 50];
            let hash = compute_data_hash(&data);
            cache.put(&CacheKey::Chunk(hash), &data).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    async fn seed_shards(cache: &LocalCache) {
        for i in 10u8..14 {
            let data = vec![i; 40];
            let hash = compute_data_hash(&data);
            cache.put(&CacheKey::Shard(hash), &data).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}
