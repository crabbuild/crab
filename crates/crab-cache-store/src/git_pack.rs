//! File-backed native Git pack routing; object selection stays in the reader.

use std::ops::Range;
use std::path::Path;

use bytes::Bytes;
use crab_storage::{StorageError, Store};
use futures_util::{StreamExt as _, TryStreamExt as _};
use tokio::io::AsyncWriteExt as _;
use tokio_util::sync::CancellationToken;

use crate::{CacheReadOutcome, CacheSource, CachingStore, Result};

const PACK_RANGE_CHUNK_BYTES: u64 = 128 * 1024 * 1024;
const PACK_RANGE_READ_CONCURRENCY: usize = 10;

/// Immutable source ranges and the authenticated identity of their Git pack body.
pub struct GitPackSource<'a> {
    pub path: &'a object_store::path::Path,
    pub size: u64,
    pub hash: &'a str,
    pub pack: Range<u64>,
    pub sidecars: Range<u64>,
    pub pack_hash: blake3::Hash,
}

/// Stage verified pack bytes and return sidecars for caller-owned Git/visibility checks.
///
/// `destination` must be an unpublished caller-owned file below an existing
/// owner-private staging directory. The selected origin, not the cache's
/// construction-time store, remains authoritative for misses.
/// Cache hits prove byte identity only; sidecars and authorization are not cached.
/// Token cancellation stops source waits and drains local writes before return.
/// Await completion before destination cleanup; dropping this future is not a drain.
pub async fn read_pack_ranges(
    origin: &Store,
    cache: Option<&CachingStore>,
    source: &GitPackSource<'_>,
    destination: &Path,
    cancel: &CancellationToken,
) -> Result<Bytes> {
    if cancel.is_cancelled() {
        return Err(StorageError::Cancelled.into());
    }
    if source.pack.start >= source.pack.end
        || source.pack.end > source.size
        || source.sidecars.start >= source.sidecars.end
        || source.sidecars.end > source.size
    {
        return Err(corrupt(source, "pack or sidecar range is outside its source").into());
    }
    let length = source.pack.end - source.pack.start;
    if let Some(cache) = cache {
        let hit = cache
            .local_cache
            .copy_git_pack_if_present(&source.pack_hash, length, destination)
            .await?;
        if cancel.is_cancelled() {
            return Err(StorageError::Cancelled.into());
        }
        cache.observe_cache_read_bytes(
            CacheSource::Local,
            if hit {
                CacheReadOutcome::Hit
            } else {
                CacheReadOutcome::Miss
            },
            if hit { length } else { 0 },
        );
        if hit {
            return read_sidecars(origin, source, cancel).await;
        }
        tokio::fs::File::create(destination)
            .await
            .map_err(StorageError::from)?;
    }

    let accelerated = origin
        .try_download_signed_ranges_to_path(
            source.path,
            destination,
            source.size,
            source.hash,
            source.pack.clone(),
            source.sidecars.clone(),
            cancel,
        )
        .await;
    let (sidecars, actual_hash) = match accelerated {
        Ok(Some(downloaded)) => downloaded,
        Ok(None) | Err(StorageError::NotSupported { .. }) => {
            // Retain the bounded parallel origin plan used by uncached clones;
            // adding retention must not serialize provider range downloads.
            let sidecars = read_sidecars(origin, source, cancel).await?;
            let ranges = (0..length)
                .step_by(PACK_RANGE_CHUNK_BYTES as usize)
                .map(|offset| {
                    let start = source.pack.start + offset;
                    let end = start
                        .saturating_add(PACK_RANGE_CHUNK_BYTES)
                        .min(source.pack.end);
                    start..end
                });
            let mut chunks = futures_util::stream::iter(ranges.map(|range| async move {
                let expected = range.end - range.start;
                let bytes = origin.range_get(source.path, range).await?;
                if bytes.len() as u64 != expected {
                    return Err(corrupt(source, "pack response has an unexpected length"));
                }
                Ok::<_, StorageError>(bytes)
            }))
            .buffered(PACK_RANGE_READ_CONCURRENCY);
            let mut output = tokio::fs::File::create(destination)
                .await
                .map_err(StorageError::from)?;
            let mut hasher = blake3::Hasher::new();
            let result = async {
                loop {
                    let chunk = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(StorageError::Cancelled),
                        chunk = chunks.try_next() => chunk?,
                    };
                    let Some(chunk) = chunk else { break };
                    output.write_all(&chunk).await.map_err(StorageError::from)?;
                    hasher.update(&chunk);
                }
                Ok::<_, StorageError>(())
            }
            .await;
            // Drain the local file even when a source fails or cancellation wins.
            let flushed = output.flush().await.map_err(StorageError::from);
            result.and(flushed)?;
            (sidecars, hasher.finalize())
        }
        Err(error) => return Err(error.into()),
    };
    if actual_hash != source.pack_hash {
        return Err(corrupt(
            source,
            "pack content hash does not match its authenticated descriptor",
        )
        .into());
    }
    if sidecars.len() as u64 != source.sidecars.end - source.sidecars.start {
        return Err(corrupt(source, "sidecar response has an unexpected length").into());
    }
    if cancel.is_cancelled() {
        return Err(StorageError::Cancelled.into());
    }
    if let Some(cache) = cache
        && let Err(error) = cache
            .local_cache
            .put_git_pack_file(&source.pack_hash, destination, length)
            .await
    {
        cache.observe_local_write_failure();
        tracing::warn!(%error, "verified Git pack remains usable after cache persistence failure");
    }
    if cancel.is_cancelled() {
        return Err(StorageError::Cancelled.into());
    }
    Ok(sidecars)
}

async fn read_sidecars(
    origin: &Store,
    source: &GitPackSource<'_>,
    cancel: &CancellationToken,
) -> Result<Bytes> {
    let bytes = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(StorageError::Cancelled.into()),
        result = origin.range_get(source.path, source.sidecars.clone()) => result?,
    };
    if bytes.len() as u64 != source.sidecars.end - source.sidecars.start {
        return Err(corrupt(source, "sidecar response has an unexpected length").into());
    }
    Ok(bytes)
}

fn corrupt(source: &GitPackSource<'_>, reason: &str) -> StorageError {
    StorageError::CorruptObject {
        path: source.path.to_string(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crab_cache::LocalCache;
    use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
    use object_store::memory::InMemory;

    use super::*;

    fn private_tempdir_in(parent: &Path) -> tempfile::TempDir {
        let mut builder = tempfile::Builder::new();
        builder.prefix("git-pack-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        builder.tempdir_in(parent).unwrap()
    }

    #[derive(Default)]
    struct Reads(Mutex<Vec<StorageObservation>>);

    impl StorageObserver for Reads {
        fn started(&self, _: StorageOperation) {}
        fn finished(&self, observation: StorageObservation) {
            if observation.bytes_read != 0 {
                self.0.lock().unwrap().push(observation);
            }
        }
    }

    #[tokio::test]
    async fn warm_pack_copy_reads_only_sidecars_from_the_selected_origin() {
        let directory = tempfile::tempdir().unwrap();
        let observer = Arc::new(Reads::default());
        let origin = Store::new(Arc::new(InMemory::new())).with_storage_observer(observer.clone());
        let path = object_store::path::Path::from("source");
        let body = Bytes::from_static(b"prefix-pack-index");
        origin.put(&path, body.clone()).await.unwrap();
        let local = Arc::new(LocalCache::with_limits(
            directory.path().join("cache"),
            Some(1024),
            None,
        ));
        let staging = private_tempdir_in(directory.path());
        // Construction-time origin has no data; routing must honor the pinned origin argument.
        let cache = CachingStore::new_with_local_cache(
            Store::new(Arc::new(InMemory::new())),
            crate::CacheConfig::default(),
            local,
        )
        .unwrap();
        let hash = blake3::hash(&body).to_hex().to_string();
        let source = GitPackSource {
            path: &path,
            size: body.len() as u64,
            hash: &hash,
            pack: 7..11,
            sidecars: 12..17,
            pack_hash: blake3::hash(b"pack"),
        };
        for (name, expected_bytes) in [
            ("cold", 9),
            ("warm", 5),
            ("warm-after-destination-write", 5),
        ] {
            observer.0.lock().unwrap().clear();
            let destination = staging.path().join(name);
            assert_eq!(
                read_pack_ranges(
                    &origin,
                    Some(&cache),
                    &source,
                    &destination,
                    &CancellationToken::new()
                )
                .await
                .unwrap(),
                b"index"[..]
            );
            assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"pack");
            assert_eq!(
                observer
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|read| read.bytes_read)
                    .sum::<u64>(),
                expected_bytes
            );
            if name == "warm" {
                tokio::fs::write(&destination, b"changed destination")
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn corrupt_origin_is_not_persisted_as_a_git_pack() {
        let directory = tempfile::tempdir().unwrap();
        let origin = Store::new(Arc::new(InMemory::new()));
        let path = object_store::path::Path::from("source");
        origin
            .put(&path, Bytes::from_static(b"bad!index"))
            .await
            .unwrap();
        let local = Arc::new(LocalCache::new(directory.path().join("cache")));
        let cache = CachingStore::new_with_local_cache(
            origin.clone(),
            crate::CacheConfig::default(),
            local.clone(),
        )
        .unwrap();
        let source = GitPackSource {
            path: &path,
            size: 9,
            hash: &"a".repeat(64),
            pack: 0..4,
            sidecars: 4..9,
            pack_hash: blake3::hash(b"pack"),
        };
        let result = read_pack_ranges(
            &origin,
            Some(&cache),
            &source,
            &directory.path().join("output"),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(crate::CacheStoreError::Storage(
                StorageError::CorruptObject { .. }
            ))
        ));
        assert_eq!(local.stats().await.unwrap().git_pack_count, 0);
    }

    #[tokio::test]
    async fn unavailable_or_full_cache_preserves_verified_origin_output() {
        let directory = tempfile::tempdir().unwrap();
        let origin = Store::new(Arc::new(InMemory::new()));
        let path = object_store::path::Path::from("source");
        let bytes = Bytes::from_static(b"packindex");
        origin.put(&path, bytes.clone()).await.unwrap();
        let source_hash = blake3::hash(&bytes).to_hex();
        let source = GitPackSource {
            path: &path,
            size: bytes.len() as u64,
            hash: source_hash.as_str(),
            pack: 0..4,
            sidecars: 4..9,
            pack_hash: blake3::hash(b"pack"),
        };
        for name in ["full", "unavailable"] {
            let root = directory.path().join(name);
            if name == "unavailable" {
                tokio::fs::write(&root, b"not a directory").await.unwrap();
            }
            let cache = CachingStore::new_with_local_cache(
                origin.clone(),
                crate::CacheConfig::default(),
                Arc::new(LocalCache::with_limits(
                    root.clone(),
                    Some(if name == "full" { 0 } else { 1024 }),
                    None,
                )),
            )
            .unwrap();
            let destination = directory.path().join(format!("{name}-output"));
            assert_eq!(
                read_pack_ranges(
                    &origin,
                    Some(&cache),
                    &source,
                    &destination,
                    &CancellationToken::new()
                )
                .await
                .unwrap(),
                b"index"[..]
            );
            assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"pack");
            assert!(!root.join("git-packs").exists());
        }
    }
}
