//! Disk-backed read-through retention for authorized full CDN LFS reads.

use std::{ops::Range, sync::Arc};

use bytes::Bytes;
use crab_cache::{
    LocalCache,
    local_cache::{LfsCacheFile, LfsCacheFill},
};
use crab_lfs::{LfsByteStream, LfsError, LfsObjectStore, LfsReadCache, LfsReadSession, Result};
use futures_util::{StreamExt as _, stream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const CACHE_READ_BYTES: u64 = 1024 * 1024;

/// Streams full LFS objects through the shared disk budget and proof cache.
pub struct LfsPayloadCache {
    root: Arc<LocalCache>,
    proofs: LfsReadCache,
}

impl LfsPayloadCache {
    /// Share the supplied root's retention limits across the authorized namespaces.
    #[must_use]
    pub fn new(root: Arc<LocalCache>) -> Self {
        Self {
            root,
            proofs: LfsReadCache::default(),
        }
    }

    /// Stream an authorized LFS object. A cache failure before delivery may use
    /// the ordinary verified reader. Partial and unbound reads bypass this cache.
    pub async fn stream_content(
        &self,
        lfs: &LfsObjectStore,
        oid: &[u8; 32],
        size: u64,
        range: Option<Range<u64>>,
    ) -> Result<(Range<u64>, LfsByteStream)> {
        let path = lfs.object_path_for(oid);
        let store = lfs.store();
        let namespace = (size != 0 && range.as_ref().is_none_or(|range| *range == (0..size)))
            .then(|| store.cdn_lfs_cache_namespace(&path, size))
            .flatten();
        let Some(namespace) = namespace else {
            return lfs.stream_content(oid, size, range, None).await;
        };
        let cache = Arc::new(self.root.for_authorized_reads(&namespace));
        match cache.open_lfs_file(oid, size).await {
            Ok(Some(source)) => {
                let (ready, selected) = oneshot::channel();
                let (send, receive) = mpsc::channel(1);
                let store = store.clone();
                tokio::spawn(deliver_cached(
                    source,
                    self.proofs.clone(),
                    *oid,
                    size,
                    send,
                    ready,
                    move || store.observe_verified_lfs_cache_hit(&path, size),
                ));
                if selected.await.is_ok() {
                    return Ok((0..size, receiver_stream(receive)));
                }
            }
            Ok(None) => {}
            Err(error) => tracing::debug!(%error, "LFS cache lookup unavailable"),
        }
        let (range, body) = lfs.stream_content(oid, size, range, None).await?;
        let body = match cache.begin_lfs_fill(oid, size).await {
            Ok(Some(fill)) => {
                let (send, receive) = mpsc::channel(1);
                tokio::spawn(retain(body, fill, size, send));
                receiver_stream(receive)
            }
            Ok(None) => body,
            Err(error) => {
                tracing::debug!(%error, "LFS cache reservation unavailable");
                body
            }
        };
        Ok((range, body))
    }
}

fn receiver_stream(receive: mpsc::Receiver<Result<Bytes>>) -> LfsByteStream {
    stream::unfold(receive, |mut receive| async move {
        receive.recv().await.map(|bytes| (bytes, receive))
    })
    .boxed()
}

async fn deliver_cached(
    source: LfsCacheFile,
    proofs: LfsReadCache,
    oid: [u8; 32],
    size: u64,
    send: mpsc::Sender<Result<Bytes>>,
    ready: oneshot::Sender<()>,
    observe: impl FnOnce(),
) {
    let session = LfsReadSession::new(proofs);
    let cancel = CancellationToken::new();
    let mut ready = Some(ready);
    let result = async {
        let mut start = 0_u64;
        loop {
            let end = size.min(start.saturating_add(CACHE_READ_BYTES));
            let file = source.reader()?;
            let bytes = tokio::select! {
                biased;
                () = send.closed() => {
                    cancel.cancel();
                    return Ok(None);
                }
                result = session.read_opened_range(source.key(), file, &oid, size, start..end, &cancel) => result?,
            };
            if let Some(ready) = ready.take()
                && ready.send(()).is_err()
            {
                return Ok(None);
            }
            if end == size {
                return Ok(Some(bytes));
            }
            if send.send(Ok(bytes)).await.is_err() {
                return Ok(None);
            }
            start = end;
        }
    }.await;
    // Keep the captured cache lease until every admitted reader has drained,
    // including a detached blocking read after the HTTP body was dropped.
    session.close().await;
    match result {
        Ok(Some(tail)) => {
            let _ = source.finish::<LfsError>(Ok(())).await;
            observe();
            let _ = send.send(Ok(tail)).await;
        }
        Ok(None) => {}
        Err(error) => {
            let error = source.finish(Err(error)).await;
            if ready.is_none()
                && let Err(error) = error
            {
                let _ = send.send(Err(error)).await;
            }
        }
    }
}

async fn retain(
    mut body: LfsByteStream,
    fill: LfsCacheFill,
    mut remaining: u64,
    send: mpsc::Sender<Result<Bytes>>,
) {
    let mut fill = Some(fill);
    loop {
        let next = tokio::select! {
            biased;
            () = send.closed() => break,
            next = body.next() => next,
        };
        let bytes = match next {
            Some(Ok(bytes)) => bytes,
            Some(Err(error)) => {
                let _ = send.send(Err(error)).await;
                break;
            }
            None => break,
        };
        if let Some(writer) = fill.as_mut()
            && let Err(error) = writer.write(&bytes).await
        {
            tracing::debug!(%error, "LFS cache write unavailable");
            abort_fill(fill.take()).await;
        }
        remaining -= bytes.len() as u64;
        if remaining == 0
            && let Some(writer) = fill.take()
            && let Err(error) = writer.commit().await
        {
            tracing::debug!(%error, "LFS cache publication unavailable");
        }
        // The LFS reader verifies size, EOF and SHA-256 before its final bytes.
        // Publish first so Content-Length clients can immediately repeat GET.
        if send.send(Ok(bytes)).await.is_err() || remaining == 0 {
            break;
        }
    }
    abort_fill(fill).await;
}

async fn abort_fill(fill: Option<LfsCacheFill>) {
    if let Some(fill) = fill
        && let Err(error) = fill.abort().await
    {
        tracing::debug!(%error, "LFS cache fill cleanup failed");
    }
}
