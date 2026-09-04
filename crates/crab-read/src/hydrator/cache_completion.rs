use std::sync::Arc;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use xet_client::cas_types::{ChunkRange, Key};
use xet_client::chunk_cache::error::ChunkCacheError;
use xet_client::chunk_cache::{CacheRange, ChunkCache};

struct OperationCache {
    inner: Arc<dyn ChunkCache>,
    cancel: CancellationToken,
    completion: Option<oneshot::Sender<()>>,
}

pub(super) fn track(
    inner: Arc<dyn ChunkCache>,
    cancel: CancellationToken,
) -> (Arc<dyn ChunkCache>, oneshot::Receiver<()>) {
    let (completion, receiver) = oneshot::channel();
    let cache = Arc::new(OperationCache {
        inner,
        cancel,
        completion: Some(completion),
    });
    (cache, receiver)
}

impl Drop for OperationCache {
    fn drop(&mut self) {
        // Xet's detached put owns a cache Arc before it is first polled. Only
        // the final Arc disappearing proves no scheduled write remains; an
        // in-flight counter incremented inside put would miss that interval.
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(());
        }
    }
}

#[async_trait::async_trait]
impl ChunkCache for OperationCache {
    async fn get(
        &self,
        key: &Key,
        range: &ChunkRange,
    ) -> Result<Option<CacheRange>, ChunkCacheError> {
        self.inner.get(key, range).await
    }

    async fn put(
        &self,
        key: &Key,
        range: &ChunkRange,
        offsets: &[u32],
        data: &[u8],
    ) -> Result<(), ChunkCacheError> {
        tokio::select! {
            biased;
            // Cache storage is best-effort. Cancellation skips publication;
            // the reconstruction boundary retains the actual read outcome.
            () = self.cancel.cancelled() => Ok(()),
            result = self.inner.put(key, range, offsets, data) => result,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    struct PendingCache {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl ChunkCache for PendingCache {
        async fn get(
            &self,
            _: &Key,
            _: &ChunkRange,
        ) -> Result<Option<CacheRange>, ChunkCacheError> {
            Ok(None)
        }

        async fn put(
            &self,
            _: &Key,
            _: &ChunkRange,
            _: &[u32],
            _: &[u8],
        ) -> Result<(), ChunkCacheError> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn completion_covers_scheduled_and_running_cache_writes() {
        for cancelled in [false, true] {
            let inner = Arc::new(PendingCache {
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            });
            let cancel = CancellationToken::new();
            let (cache, mut completion) = track(inner.clone(), cancel.clone());
            let queued = Arc::clone(&cache);
            drop(cache);
            assert_eq!(
                completion.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            );

            let write = tokio::spawn(async move {
                let key = Key {
                    prefix: "test".into(),
                    hash: crab_xet::hash::MerkleHash::from([1, 2, 3, 4]),
                };
                queued
                    .put(&key, &ChunkRange::new(0, 1), &[0, 1], b"x")
                    .await
            });
            inner.entered.notified().await;
            assert_eq!(
                completion.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            );
            if cancelled {
                cancel.cancel();
            } else {
                inner.release.notify_one();
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), completion)
                .await
                .unwrap()
                .unwrap();
            write.await.unwrap().unwrap();
        }
    }
}
