use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use xet_client::cas_types::{ChunkRange, Key};
use xet_client::chunk_cache::error::ChunkCacheError;
use xet_client::chunk_cache::{CacheRange, ChunkCache};

use super::tests::reconstruction_fixture;

#[derive(Default)]
struct ControlledCache {
    entered: Notify,
    release: Notify,
    stopped: Notify,
    fail: bool,
}

struct WriteLifetime<'a>(&'a Notify);

impl Drop for WriteLifetime<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[async_trait::async_trait]
impl ChunkCache for ControlledCache {
    async fn get(&self, _: &Key, _: &ChunkRange) -> Result<Option<CacheRange>, ChunkCacheError> {
        Ok(None)
    }

    async fn put(
        &self,
        _: &Key,
        _: &ChunkRange,
        _: &[u32],
        _: &[u8],
    ) -> Result<(), ChunkCacheError> {
        let _lifetime = WriteLifetime(&self.stopped);
        self.entered.notify_one();
        self.release.notified().await;
        if self.fail {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into());
        }
        Ok(())
    }
}

#[tokio::test]
async fn reconstruction_awaits_cache_attempts_without_promoting_cache_errors() {
    for fail in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (mut hydrator, pointer, original) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        let cache = Arc::new(ControlledCache {
            fail,
            ..ControlledCache::default()
        });
        hydrator.chunk_cache = Some(cache.clone());
        let mut read = tokio::spawn(async move {
            hydrator
                .reconstruct_from_pointer(&pointer.serialize())
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), cache.entered.notified())
            .await
            .unwrap();
        // The real Xet task has reached put, but its durable attempt is held.
        // Completing reconstruction here would let process exit lose the fill.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut read)
                .await
                .is_err()
        );
        cache.release.notify_one();
        let bytes = tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(bytes, original);
    }
}

#[tokio::test]
async fn cancellation_and_drop_stop_pending_cache_publication() {
    for abort in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (mut hydrator, pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        let cache = Arc::new(ControlledCache::default());
        hydrator.chunk_cache = Some(cache.clone());
        let cancel = CancellationToken::new();
        let read_cancel = cancel.clone();
        let read = tokio::spawn(async move {
            hydrator
                .reconstruct_to_writer_with_cancel(&pointer, std::io::sink(), None, &read_cancel)
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), cache.entered.notified())
            .await
            .unwrap();
        if abort {
            read.abort();
            assert!(read.await.unwrap_err().is_cancelled());
        } else {
            cancel.cancel();
            let result = tokio::time::timeout(Duration::from_secs(5), read)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(result, Err(crate::ReadError::Cancelled)));
        }
        tokio::time::timeout(Duration::from_secs(5), cache.stopped.notified())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn cache_fill_finishes_before_decoded_bytes_leave_admission() {
    struct ObservedWriter(Arc<std::sync::atomic::AtomicUsize>);
    impl std::io::Write for ObservedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .fetch_add(bytes.len(), std::sync::atomic::Ordering::SeqCst);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let (mut hydrator, pointer, _) =
        reconstruction_fixture(&directory.path().join("cache"), false).await;
    let cache = Arc::new(ControlledCache::default());
    hydrator.chunk_cache = Some(cache.clone());
    let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let writer = ObservedWriter(written.clone());
    let read = tokio::spawn(async move {
        hydrator
            .reconstruct_to_writer_with_cancel(&pointer, writer, None, &CancellationToken::new())
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), cache.entered.notified())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let bytes_before_fill = written.load(std::sync::atomic::Ordering::SeqCst);
    cache.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(bytes_before_fill, 0);
}
