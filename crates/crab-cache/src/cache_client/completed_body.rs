//! Bounded HTTP completion before handing an immutable stream to its consumer.

use std::io::{Read as _, Seek as _, SeekFrom, Write as _};

use bytes::Bytes;
use futures_util::{StreamExt as _, stream::BoxStream};

use super::CacheObjectStream;
use crate::catalog::{CacheCatalog, CacheReservation};
use crate::{CacheError, LocalCache, Result};

const READ_BYTES: usize = 64 * 1024;

struct CompletedBody {
    // Drop the file before releasing its charge. Each blocking I/O task owns
    // both fields, so cancelling its waiter cannot release capacity early.
    file: std::fs::File,
    _reservation: CacheReservation,
}

pub(super) async fn complete(
    response: CacheObjectStream,
    cache: &LocalCache,
) -> Result<Option<BoxStream<'static, Result<Bytes>>>> {
    let size = response
        .content_length()
        .ok_or_else(|| CacheError::Service {
            reason: "cache service omitted Content-Length for streamed object".into(),
        })?;
    let catalog = CacheCatalog::new(cache.root().to_owned(), cache.max_bytes());
    // This logical reservation name is never published. Concurrent bodies
    // have independent reservation IDs and anonymous file descriptors.
    let path = cache.root().join(".streamed-object");
    let Some(reservation) = catalog.reserve(&path, size).await? else {
        return Ok(None);
    };
    let (file, reservation) = reservation.anonymous_file().await?;
    let mut completed = CompletedBody {
        file,
        _reservation: reservation,
    };
    let mut source = response.into_stream().boxed();
    while let Some(chunk) = source.next().await {
        let chunk = chunk?;
        completed = tokio::task::spawn_blocking(move || {
            completed.file.write_all(&chunk)?;
            Ok::<_, CacheError>(completed)
        })
        .await
        .map_err(join_error)??;
    }
    completed = tokio::task::spawn_blocking(move || {
        completed.file.seek(SeekFrom::Start(0))?;
        Ok::<_, CacheError>(completed)
    })
    .await
    .map_err(join_error)??;

    Ok(Some(
        futures_util::stream::unfold(Some(completed), |state| async move {
            let mut completed = state?;
            let result = tokio::task::spawn_blocking(move || {
                let mut buffer = vec![0; READ_BYTES];
                let read = completed.file.read(&mut buffer)?;
                buffer.truncate(read);
                Ok::<_, CacheError>((Bytes::from(buffer), completed))
            })
            .await
            .map_err(join_error)
            .and_then(|result| result);
            match result {
                Ok((bytes, _)) if bytes.is_empty() => None,
                Ok((bytes, completed)) => Some((Ok(bytes), Some(completed))),
                Err(error) => Some((Err(error), None)),
            }
        })
        .boxed(),
    ))
}

fn join_error(error: tokio::task::JoinError) -> CacheError {
    CacheError::Io(std::io::Error::other(error))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::{CacheClient, CacheServiceAuth};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    async fn response(body: Bytes, advertised: usize) -> CacheObjectStream {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {advertised}\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        });
        let client = CacheClient::new(
            &format!("http://{address}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();
        let response = client
            .get_stream("repo/packs/pack-test.pack")
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
        response
    }

    fn reserved(cache: &LocalCache) -> u64 {
        CacheCatalog::read_only_stats(cache.root())
            .unwrap()
            .reservations_bytes
    }

    #[tokio::test]
    async fn completed_stream_retains_budget_until_consumed_or_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let expected = Bytes::from(vec![0x3d; READ_BYTES * 3 + 1]);
        // The catalog also occupies this root. Leave room for its files while
        // keeping the budget strictly below two complete response bodies.
        let cache = LocalCache::with_limits(
            directory.path().join("cache"),
            expected.len() as u64 * 2 - 1,
            None,
        );
        let mut completed = response(expected.clone(), expected.len())
            .await
            .complete(&cache)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reserved(&cache), expected.len() as u64);
        assert!(
            response(expected.clone(), expected.len())
                .await
                .complete(&cache)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!cache.root().join(".streamed-object").exists());
        assert!(std::fs::read_dir(cache.root()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-")
        }));

        let mut actual = Vec::new();
        while let Some(chunk) = completed.next().await {
            let chunk = chunk.unwrap();
            assert!(chunk.len() <= READ_BYTES);
            actual.extend_from_slice(&chunk);
        }
        assert_eq!(actual, expected);
        assert_eq!(reserved(&cache), 0);

        let completed = response(expected.clone(), expected.len())
            .await
            .complete(&cache)
            .await
            .unwrap()
            .unwrap();
        drop(completed);
        assert_eq!(reserved(&cache), 0);
    }

    #[tokio::test]
    async fn incomplete_response_releases_its_anonymous_file_and_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(directory.path().join("cache"));
        let result = response(Bytes::from_static(b"incomplete"), 100)
            .await
            .complete(&cache)
            .await;
        assert!(result.is_err());
        assert_eq!(reserved(&cache), 0);
        assert!(std::fs::read_dir(cache.root()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-")
        }));
    }

    #[tokio::test]
    async fn over_budget_response_bypasses_without_initializing_disk_state() {
        let directory = tempfile::tempdir().unwrap();
        let cache = LocalCache::with_limits(directory.path().join("cache"), 1, None);
        let result = response(Bytes::from_static(b"large"), 5)
            .await
            .complete(&cache)
            .await
            .unwrap();
        assert!(result.is_none());
        assert!(!cache.root().exists());
    }

    #[tokio::test]
    async fn cancellation_during_body_releases_reserved_capacity() {
        use std::sync::Arc;
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(directory.path().join("cache")));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial",
                )
                .await
                .unwrap();
            let _ = released.await;
        });
        let client = CacheClient::new(
            &format!("http://{address}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();
        let response = client
            .get_stream("repo/packs/pack-test.pack")
            .await
            .unwrap()
            .unwrap();
        let worker_cache = Arc::clone(&cache);
        let completion = tokio::spawn(async move { response.complete(&worker_cache).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !CacheCatalog::read_only_stats(cache.root())
                .is_ok_and(|stats| stats.reservations_bytes == 100)
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        completion.abort();
        assert!(matches!(completion.await, Err(error) if error.is_cancelled()));
        let _ = release.send(());
        server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while reserved(&cache) != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(reserved(&cache), 0);
    }
}
