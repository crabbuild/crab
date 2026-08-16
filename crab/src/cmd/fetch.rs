//! `crab fetch` — pre-fetch objects from the remote store into the local cache.
//!
//! Downloads xorbs and shards referenced by the current HEAD (or a
//! specified ref) without hydrating files. This warms the local cache
//! so subsequent `crab hydrate` or `git checkout` operations are fast.

use std::future::Future;
use std::io::Stdout;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::event_payloads::{
    PERF_PHASE_SCHEMA, PerfPhasePayload, ProgressPayload, XorbDonePayload,
};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::core::perf_phase::PhaseTimer;

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
    /// Skip the post-fetch shard-sync step that warms the local
    /// chunk-index cache. CI workloads that push once and never read
    /// back can use this to avoid the warming cost.
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
    let cwd = std::env::current_dir()?;
    run_fetch_in(&cwd, args, cancel).await
}

fn emit_phase(stream: Option<&Mutex<JsonlStream<Stdout>>>, payload: PerfPhasePayload) {
    if let Some(stream) = stream {
        let Ok(mut s) = stream.lock() else {
            return;
        };
        s.emit_schema_event(PERF_PHASE_SCHEMA, "event", payload);
    }
}

/// Fetch objects for the repository rooted at `root`.
///
/// Reads the remote URL from `.crab/remote`, resolves the current
/// HEAD's tree, and downloads any missing xorbs/shards into the local
/// cache directory.
pub async fn run_fetch_in(root: &Path, args: &FetchArgs, cancel: &CancellationToken) -> Result<()> {
    let config = Config::resolve_local().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to load config, using defaults");
        Config::default()
    });

    run_fetch_in_with_selector(
        root,
        args,
        cancel,
        config,
        |config, parsed, cancel| async move {
            crate::replication::select_read_store(&config, &parsed, "fetch", &cancel).await
        },
    )
    .await
}

async fn run_fetch_in_with_selector<F, Fut>(
    root: &Path,
    args: &FetchArgs,
    cancel: &CancellationToken,
    config: Config,
    select_read: F,
) -> Result<()>
where
    F: FnOnce(Config, crate::git::url::CrabUrl, tokio_util::sync::CancellationToken) -> Fut,
    Fut: Future<Output = Result<crate::replication::ReadStoreSelection>>,
{
    use futures_util::TryStreamExt;

    let start = Instant::now();

    check_cancelled(cancel)?;

    let remote_path = root.join(".crab/remote");
    let url = std::fs::read_to_string(&remote_path).map_err(|_| CrabError::Configuration {
        key: "no remote configured".into(),
        origin: remote_path.display().to_string(),
    })?;
    let url = url.trim();

    let parsed = crate::git::url::CrabUrl::parse(url)?;

    tracing::info!(
        bucket = %parsed.bucket,
        repo_path = %parsed.repo_path,
        all = args.all,
        dry_run = args.dry_run,
        "starting fetch",
    );

    check_cancelled(cancel)?;

    let selection = select_read(config.clone(), parsed.clone(), cancel.clone()).await?;
    let crate::replication::ReadStoreSelection {
        store,
        router: read_router,
        source,
    } = selection;
    if let crate::replication::ReadSource::Replica { name } = &source {
        tracing::debug!(replica = %name, "selected read replica for fetch");
    }

    // Wrap with CachingStore when a cache service is configured.
    let caching_store = crab_cache_store::CachingStore::new(store, &config.cache)?;

    // List objects under the repo prefix to discover what needs fetching.
    let prefix = object_store::path::Path::from(parsed.repo_path.as_str());

    if args.dry_run {
        let summary = FetchSummary {
            objects_fetched: 0,
            bytes_downloaded: 0,
            objects_skipped: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
        match args.mode {
            OutputMode::Text => {
                eprintln!("fetch (dry run): would fetch objects from {url}");
                eprintln!("  prefix: {prefix}");
                eprintln!("  include: {:?}", args.include);
                eprintln!("  exclude: {:?}", args.exclude);
            }
            OutputMode::Json => {
                emit_json("fetch", "1.0", &summary);
            }
            OutputMode::Jsonl => {
                let mut stream = JsonlStream::new("fetch.event", "1.0", std::io::stdout());
                stream.emit_result(&summary);
            }
        }
        return Ok(());
    }

    // Build the optional JSONL stream for streaming mode.
    let jsonl_stream: Option<Mutex<JsonlStream<Stdout>>> = match args.mode {
        OutputMode::Jsonl => Some(Mutex::new(JsonlStream::new(
            "fetch.event",
            "1.0",
            std::io::stdout(),
        ))),
        _ => None,
    };

    // Fetch shard metadata first (from global prefix), then xorbs (from global prefix).
    // Packs remain at the per-repo prefix.
    let global_shards_prefix = object_store::path::Path::from(".crab/shards");
    let global_xorbs_prefix = object_store::path::Path::from(".crab/xorbs");

    let mut fetched_count: u64 = 0;
    let mut fetched_bytes: u64 = 0;
    let mut skipped_count: u64 = 0;

    let phase = PhaseTimer::start("fetch", "hydration_prefetch");
    for obj_prefix in [&global_shards_prefix, &global_xorbs_prefix] {
        check_cancelled(cancel)?;

        let mut list_stream = caching_store.origin().inner().list(Some(obj_prefix));
        while let Some(meta) = list_stream.try_next().await.map_err(CrabError::Storage)? {
            check_cancelled(cancel)?;

            let cache_path = cache_path_for(&meta.location);
            if cache_path.exists() {
                tracing::debug!(path = %meta.location, "already cached, skipping");
                skipped_count += 1;
                continue;
            }

            tracing::debug!(path = %meta.location, size = meta.size, "fetching");
            let (data, _etag) = caching_store.get_with_etag(&meta.location).await?;

            // Ensure parent directory exists.
            if let Some(parent) = cache_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&cache_path, &data)?;

            let obj_bytes = data.len() as u64;
            fetched_count += 1;
            fetched_bytes += obj_bytes;

            // Emit xorb_done and progress events in JSONL mode.
            if let Some(ref stream) = jsonl_stream
                && let Ok(mut s) = stream.lock()
            {
                s.emit_xorb_done(XorbDonePayload {
                    hash: meta.location.to_string(),
                    bytes: obj_bytes,
                    compressed_bytes: obj_bytes,
                    status: "ok".to_owned(),
                });

                let elapsed = start.elapsed();
                let rate = if elapsed.as_secs_f64() > 0.0 {
                    fetched_bytes as f64 / elapsed.as_secs_f64()
                } else {
                    0.0
                };
                s.emit_progress(ProgressPayload {
                    operation: "fetching".to_owned(),
                    current: fetched_count,
                    total: 0,
                    bytes: fetched_bytes,
                    total_bytes: 0,
                    rate_bytes_per_sec: rate,
                    xorbs_produced: None,
                });
            }
        }
    }
    emit_phase(
        jsonl_stream.as_ref(),
        phase.finish(0, fetched_bytes, fetched_count),
    );

    let elapsed = start.elapsed();
    let summary = FetchSummary {
        objects_fetched: fetched_count,
        bytes_downloaded: fetched_bytes,
        objects_skipped: skipped_count,
        duration_ms: elapsed.as_millis() as u64,
    };

    // After the packs/xorbs/shards are on disk, warm the local
    // chunk-index cache so the next push can classify most chunks as
    // already-remote from the local tiers alone. Failures are
    // non-fatal: the cache is an optimisation, correctness comes from
    // lazy-on-miss lookups against the remote chunk_index_db.
    if !args.no_sync_chunk_index && !args.dry_run {
        check_cancelled(cancel)?;
        let phase = PhaseTimer::start("fetch", "shard_sync");

        let router = crate::storage::StoreLayout::new(
            crate::storage::Store::from_storage(caching_store.origin().clone()),
            read_router.repo_prefix().to_owned(),
        );
        let cache_dir = crate::cache::default_cache_root();
        let repo_hash = crate::git::push::compute_repo_hash(&parsed.repo_path);

        let emit_text = matches!(args.mode, OutputMode::Text);
        match crate::metadata::shard_sync::run_post_fetch_shard_sync(
            router, &repo_hash, &cache_dir, None, emit_text,
        )
        .await
        {
            Ok(stats) => {
                tracing::debug!(
                    downloaded = stats.shards_downloaded,
                    skipped = stats.shards_skipped,
                    failed = stats.shards_failed,
                    "fetch: post-fetch shard sync finished"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "fetch: post-fetch shard sync failed (non-fatal)");
            }
        }
        emit_phase(jsonl_stream.as_ref(), phase.finish(0, 0, 1));
    }

    match args.mode {
        OutputMode::Text => {
            eprintln!("fetch complete: {fetched_count} objects, {fetched_bytes} bytes downloaded",);
        }
        OutputMode::Json => {
            emit_json("fetch", "1.0", &summary);
        }
        OutputMode::Jsonl => {
            if let Some(ref stream) = jsonl_stream
                && let Ok(mut s) = stream.lock()
            {
                s.emit_result(&summary);
            }
        }
    }

    Ok(())
}

/// Derive a local cache path for a remote object.
fn cache_path_for(location: &object_store::path::Path) -> std::path::PathBuf {
    let cache_root = crate::cache::default_cache_root();
    cache_root.join(location.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use std::sync::Arc;

    #[test]
    fn cache_path_includes_object_location() {
        let loc = object_store::path::Path::from("repo/shards/abc123");
        let path = cache_path_for(&loc);
        assert!(
            path.to_string_lossy().contains("repo/shards/abc123"),
            "cache path should include the object location, got: {}",
            path.display(),
        );
    }

    #[tokio::test]
    async fn fetch_command_uses_selected_replica_store_for_cached_objects() {
        let cache_tmp = tempfile::tempdir().expect("cache tempdir");
        let _cache_guard = crate::test::git_repo::CacheDirGuard::new(cache_tmp.path());
        let root = tempfile::tempdir().expect("workspace tempdir");
        std::fs::create_dir_all(root.path().join(".crab")).expect("create .crab");
        std::fs::write(
            root.path().join(".crab/remote"),
            "crab://primary/org/repo\n",
        )
        .expect("write remote");

        let primary = crate::storage::store::Store::new(Arc::new(InMemory::new()));
        primary
            .put(
                &ObjectPath::from(".crab/shards/only-primary"),
                Bytes::from_static(b"primary"),
            )
            .await
            .expect("write primary marker");
        let replica = crate::storage::store::Store::new(Arc::new(InMemory::new()));
        replica
            .put(
                &ObjectPath::from(".crab/shards/only-replica"),
                Bytes::from_static(b"replica"),
            )
            .await
            .expect("write replica marker");

        let args = FetchArgs {
            include: Vec::new(),
            exclude: Vec::new(),
            all: false,
            dry_run: false,
            no_sync_chunk_index: true,
            mode: OutputMode::Json,
        };
        let cancel = CancellationToken::new();

        run_fetch_in_with_selector(
            root.path(),
            &args,
            &cancel,
            Config::default(),
            move |_, _, _| {
                let replica = replica.clone();
                async move {
                    Ok(crate::replication::ReadStoreSelection {
                        store: replica.clone(),
                        router: crate::storage::StoreLayout::new(replica, "org/repo".into()),
                        source: crate::replication::ReadSource::Replica {
                            name: "west".into(),
                        },
                    })
                }
            },
        )
        .await
        .expect("fetch command");

        assert_eq!(
            std::fs::read(cache_tmp.path().join(".crab/shards/only-replica"))
                .expect("replica marker cached"),
            b"replica"
        );
        assert!(
            !cache_tmp.path().join(".crab/shards/only-primary").exists(),
            "fetch must consume selected replica store, not the primary fallback"
        );
    }
}
