//! Shared-owner baseline for complete Crab-pointer reconstruction.
use crab_remote_git::{
    NoopMetrics, OperationKind, RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity,
    RepositoryOptions, RuntimeOptions,
};
use std::{io::Write, path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

type Failure = Box<dyn std::error::Error>;

struct HashWriter {
    hash: blake3::Hasher,
    result: Option<tokio::sync::oneshot::Sender<blake3::Hash>>,
}
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for HashWriter {
    fn drop(&mut self) {
        if let Some(result) = self.result.take() {
            let _ = result.send(self.hash.finalize());
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [bucket, repository, branch, path, mode, cache, rest @ ..] = args.as_slice() else {
        return Err(
            "usage: core_crab_read BUCKET REPOSITORY BRANCH PATH hydrated CACHE [CACHE_BYTES]"
                .into(),
        );
    };
    if mode != "hydrated" {
        return Err("baseline requires hydrated Crab content".into());
    }
    if rest.len() > 1 {
        return Err("too many cache arguments".into());
    }
    let cache_bytes = rest
        .first()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(512 * 1024 * 1024);
    if cache_bytes == 0 {
        return Err("cache budget must be nonzero".into());
    }
    let cache = PathBuf::from(cache).canonicalize()?;
    if !cache.is_dir() {
        return Err("cache must be an existing directory".into());
    }
    let store = crab_storage::provider_store::build_static_env_target_store(
        crab_storage::StaticEnvStoreTarget::bucket(
            crab_storage::StorageProviderKind::S3,
            bucket.clone(),
        ),
    )?;
    let namespace = blake3::Hash::from(*store.target_identity().ok_or("missing store identity")?)
        .to_hex()
        .to_string();
    let layout = crab_storage::StoreLayout::new(store.clone(), repository.clone());
    let runtime = Arc::new(RemoteGitRuntime::new(
        RuntimeOptions::default(),
        Arc::new(NoopMetrics),
    )?);
    let cancel = CancellationToken::new();
    let result = async {
        let owner = RemoteGitRepository::open(
            store,
            layout.clone(),
            RepositoryIdentity::new(namespace.clone(), repository.clone(), 1)?,
            runtime.clone(),
            RepositoryOptions::default(),
            &cancel,
        )
        .await?;
        let operation = owner.operation(OperationKind::Snapshot, &cancel).await?;
        let snapshot = owner
            .snapshot(
                &crab_remote_git::Revision::Reference(format!("refs/heads/{branch}")),
                &operation,
            )
            .await;
        let snapshot = operation.finish(snapshot).await?;
        let limits = crab_remote_git::OperationLimits {
            max_fetched_bytes: 4 * 1024 * 1024 * 1024,
            max_duration: Duration::from_secs(120),
            ..Default::default()
        };
        let operation = owner
            .operation_with_limits(OperationKind::Content, &cancel, limits)
            .await?;
        let result = async {
            let path = crab_remote_git::GitPath::new(path.as_bytes().to_vec())?;
            let blob = snapshot.read_blob(&path, &operation).await?;
            let pointer = crab_types::pointer::Pointer::parse(&blob.bytes)?;
            operation
                .charge(
                    crab_remote_git::BudgetDimension::ResponseBytes,
                    pointer.size,
                )
                .await?;
            let mut key = blake3::Hasher::new();
            key.update(namespace.as_bytes());
            key.update(layout.repo_prefix().as_bytes());
            let root = cache.join(key.finalize().to_hex().as_str());
            let worker_layout = layout.clone();
            let hydrator = tokio::task::spawn_blocking(move || {
                let local = Arc::new(crab_cache::LocalCache::with_limits(
                    root,
                    cache_bytes,
                    Some(cache_bytes),
                ));
                let store = crab_cache_store::CachingStore::new_with_local_cache(
                    worker_layout.store().clone(),
                    crab_cache_store::CacheConfig {
                        max_bytes: Some(cache_bytes),
                        ..Default::default()
                    },
                    local,
                )?;
                crab_read::ReadRuntimeBuilder::new(
                    store,
                    worker_layout,
                    RuntimeOptions::default().max_origin_concurrency,
                )
                .build()
            })
            .await??
            .with_read_admission(operation.read_admission());
            let lookup = hydrator.file_index_lookup(
                owner.shard_index_hash().to_owned(),
                owner.generation(),
                crab_metadata::file_index_lookup::FileIndexLookupLimits {
                    max_files: 10_000,
                    max_shard_visits: 100_000,
                    max_shard_bytes: 4 * 1024 * 1024 * 1024,
                    max_recipe_entries: 100_000,
                },
            );
            let (send, hash) = tokio::sync::oneshot::channel();
            let size = hydrator
                .reconstruct_to_writer_with_cancel(
                    &pointer,
                    HashWriter {
                        hash: blake3::Hasher::new(),
                        result: Some(send),
                    },
                    Some(&lookup),
                    operation.cancellation(),
                )
                .await;
            let close = lookup.close().await;
            if let (Err(_), Err(error)) = (&size, &close) {
                eprintln!("additional lookup cleanup failure: {error:?}");
            }
            let size = size?;
            close?;
            Ok::<_, Failure>((size, hash.await?))
        }
        .await;
        // Finish the owner operation even when reconstruction or lookup close fails.
        let close = operation.finish(Ok(())).await;
        if let (Err(_), Err(error)) = (&result, &close) {
            eprintln!("additional operation cleanup failure: {error:?}");
        }
        let (size, hash) = result?;
        close?;
        writeln!(
            std::io::stdout(),
            "commit={} bytes={} blake3={}",
            snapshot.commit_oid(),
            size,
            hash
        )?;
        Ok::<_, Failure>(())
    }
    .await;
    runtime.shutdown().await;
    result
}
