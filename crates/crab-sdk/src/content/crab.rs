use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use crab_remote_git::OperationContext;
use crab_storage::{Store, StoreLayout};
use tokio::sync::{OnceCell, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::remote_error::{consumer_error, metadata_kind, storage_kind};
use crate::{Error, ErrorKind, ReadOptions, Result};

/// Explicit cache directory and finite retention budget for Crab reconstruction.
#[derive(Clone, Debug)]
pub struct ContentCache {
    root: PathBuf,
    max_bytes: u64,
}

impl ContentCache {
    /// Select an existing absolute directory and a nonzero byte budget.
    pub fn new(root: &Path, max_bytes: u64) -> Result<Self> {
        if !root.is_absolute() || max_bytes == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "content cache requires an absolute directory and nonzero budget",
            ));
        }
        let root = root.canonicalize().map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot resolve content cache", source)
        })?;
        if !root.is_dir() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "content cache is not a directory",
            ));
        }
        Ok(Self { root, max_bytes })
    }
}

pub(crate) struct ContentRuntime {
    pub(crate) layout: StoreLayout<Store>,
    cache: Option<ContentCache>,
    namespace: String,
    shard_index_hash: String,
    generation: u64,
    hydrator: OnceCell<crab_read::ShardHydrator>,
}

impl ContentRuntime {
    pub(crate) fn new(
        cache: Option<ContentCache>,
        layout: StoreLayout<Store>,
        namespace: &str,
        shard_index_hash: String,
        generation: u64,
    ) -> Self {
        let mut hash = blake3::Hasher::new();
        hash.update(namespace.as_bytes());
        hash.update(layout.repo_prefix().as_bytes());
        Self {
            layout,
            cache,
            shard_index_hash,
            generation,
            namespace: hash.finalize().to_hex().to_string(),
            hydrator: OnceCell::new(),
        }
    }

    async fn hydrator(&self) -> Result<&crab_read::ShardHydrator> {
        self.hydrator
            .get_or_try_init(|| async {
                let cache = self.cache.clone().ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        "Crab reconstruction requires an explicit content cache",
                    )
                })?;
                let root = cache.root.join(&self.namespace);
                let layout = self.layout.clone();
                // Cache initialization can touch disk; the tracked caller awaits this
                // job rather than abandoning it when a public request is dropped.
                tokio::task::spawn_blocking(move || {
                    let local = Arc::new(crab_cache::LocalCache::with_limits(
                        root,
                        cache.max_bytes,
                        Some(cache.max_bytes),
                    ));
                    let config = crab_cache_store::CacheConfig {
                        max_bytes: Some(cache.max_bytes),
                        ..Default::default()
                    };
                    let store = crab_cache_store::CachingStore::new_with_local_cache(
                        layout.store().clone(),
                        config,
                        local,
                    )
                    .map_err(|source| {
                        Error::with_source(ErrorKind::Io, "cannot initialize content cache", source)
                    })?;
                    crab_read::ReadRuntimeBuilder::new(
                        store,
                        layout,
                        crab_remote_git::RuntimeOptions::default().max_origin_concurrency,
                    )
                    .build()
                    .map_err(read_error)
                })
                .await
                .map_err(|source| {
                    Error::with_source(
                        ErrorKind::Io,
                        "content initialization worker failed",
                        source,
                    )
                })?
            })
            .await
    }

    pub(crate) async fn deliver(
        &self,
        bytes: Bytes,
        options: ReadOptions,
        operation: &OperationContext,
        send: mpsc::Sender<Bytes>,
        ready: oneshot::Sender<()>,
    ) -> crab_remote_git::Result<()> {
        let pointer = crab_types::pointer::Pointer::parse(&bytes).map_err(|source| {
            consumer_error(Error::with_source(
                ErrorKind::Corruption,
                "invalid Crab pointer",
                source,
            ))
        })?;
        let range = options.byte_range(pointer.size).map_err(consumer_error)?;
        let hydrator = self
            .hydrator()
            .await
            .map_err(consumer_error)?
            .clone()
            .with_read_admission(operation.read_admission());
        let limits = options.read_limits();
        let lookup = hydrator.file_index_lookup(
            self.shard_index_hash.clone(),
            self.generation,
            crab_metadata::file_index_lookup::FileIndexLookupLimits {
                max_files: usize::try_from(limits.max_logical_objects).unwrap_or(usize::MAX),
                max_shard_visits: usize::try_from(limits.max_entries).unwrap_or(usize::MAX),
                max_shard_bytes: limits.max_fetched_bytes,
                max_recipe_entries: usize::try_from(limits.max_entries).unwrap_or(usize::MAX),
            },
        );
        let writer = ChannelWriter {
            send,
            cancellation: operation.cancellation().clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        let _ = ready.send(());
        let result = if range == (0..pointer.size) {
            hydrator
                .reconstruct_to_writer_with_cancel(
                    &pointer,
                    writer,
                    Some(&lookup),
                    operation.cancellation(),
                )
                .await
        } else {
            hydrator
                .reconstruct_range_to_writer_with_cancel(
                    &pointer,
                    range,
                    writer,
                    Some(&lookup),
                    operation.cancellation(),
                )
                .await
        };
        // The lookup belongs to this reconstruction, including fallback from a
        // stale hint. Finish it before finalizing the Git operation.
        let result = result.map(|_| ()).map_err(read_error);
        let close = lookup.close().await.map_err(|source| {
            Error::with_source(
                metadata_kind(&source),
                "content lookup close failed",
                source,
            )
        });
        let result = match (result, close) {
            (Ok(()), result) => result,
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(close)) => Err(error.with_cleanup(close)),
        };
        result.map_err(|error| {
            if error.kind() == ErrorKind::Cancelled && error.cleanup_error().is_none() {
                crab_remote_git::Error::Cancelled
            } else {
                consumer_error(error)
            }
        })
    }
}

struct ChannelWriter {
    send: mpsc::Sender<Bytes>,
    cancellation: CancellationToken,
    runtime: tokio::runtime::Handle,
}

impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = bytes.len().min(super::OUTPUT_CHUNK_BYTES);
        let chunk = Bytes::copy_from_slice(&bytes[..count]);
        // Xet invokes Write on its blocking writer. Cancellation must unblock
        // channel backpressure even when the consumer keeps its receiver alive.
        self.runtime.block_on(async {
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => Err(cancelled_write()),
                result = self.send.send(chunk) => result.map_err(|_| cancelled_write()),
            }
        })?;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn cancelled_write() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        crab_read::ReadError::Cancelled,
    )
}

fn read_error(source: crab_read::ReadError) -> Error {
    let mut kind = ErrorKind::Io;
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(&source);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<crab_metadata::error::MetadataError>() {
            kind = metadata_kind(error);
            break;
        }
        if let Some(error) = error.downcast_ref::<crab_storage::StorageError>() {
            kind = storage_kind(error);
            break;
        }
        if matches!(
            error.downcast_ref::<crab_cache_store::CacheStoreError>(),
            Some(crab_cache_store::CacheStoreError::OriginIntegrity { .. })
        ) {
            kind = ErrorKind::Corruption;
            break;
        }
        if let Some(error) = error.downcast_ref::<crab_read::ReadError>() {
            use crab_read::ReadError as R;
            let category = match error {
                R::Cancelled => Some(ErrorKind::Cancelled),
                R::Pointer(_)
                | R::CorruptObject { .. }
                | R::HashMismatch { .. }
                | R::IncompleteShardReconstruction { .. } => Some(ErrorKind::Corruption),
                R::NotFound { .. } => Some(ErrorKind::NotFound),
                R::UnauthorizedObject => Some(ErrorKind::Authorization),
                R::Configuration { .. } => Some(ErrorKind::InvalidInput),
                _ => None,
            };
            if let Some(category) = category {
                kind = category;
                break;
            }
        }
        // std::io::Error::source skips its boxed payload. Inspect that payload
        // so the bridge's typed cancellation survives the writer-error boundary.
        current = error
            .downcast_ref::<std::io::Error>()
            .and_then(|error| {
                error
                    .get_ref()
                    .map(|source| source as &(dyn std::error::Error + 'static))
            })
            .or_else(|| error.source());
    }
    Error::with_source(kind, "Crab content reconstruction failed", source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_releases_blocked_writer_without_dropping_receiver() {
        let (send, mut receive) = mpsc::channel(1);
        send.send(Bytes::from_static(b"queued")).await.unwrap();
        let cancellation = CancellationToken::new();
        let mut writer = ChannelWriter {
            send,
            cancellation: cancellation.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        let (started, entered) = oneshot::channel();
        let job = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            writer.write(b"blocked")
        });
        entered.await.unwrap();
        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(2), job)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(
            read_error(crab_read::ReadError::Io(error)).kind(),
            ErrorKind::Cancelled
        );
        assert_eq!(receive.recv().await.unwrap().as_ref(), b"queued");
        assert!(receive.recv().await.is_none());
    }
}
