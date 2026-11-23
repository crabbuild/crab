use std::sync::Arc;

use bytes::Bytes;
use crab_cache::{LocalCache, default_cache_root};
use crab_storage::{Store, cdn::CapsuleControlCache};

struct LocalControls(Arc<LocalCache>);

#[async_trait::async_trait]
impl CapsuleControlCache for LocalControls {
    async fn read(&self, key: &blake3::Hash, bytes: u64) -> Option<Bytes> {
        match self.0.read_capsule_control(key, bytes).await {
            Ok(result) => result,
            Err(error) => {
                tracing::debug!(%error, "capsule control cache read failed");
                None
            }
        }
    }

    async fn retain(&self, key: &blake3::Hash, bytes: &Bytes) {
        if let Err(error) = self.0.put_capsule_control(key, bytes).await {
            tracing::debug!(%error, "capsule control cache retention failed");
        }
    }
}

/// Attach local capsule retention to a selected native reader without wrapping its transport.
#[must_use]
pub fn cache_capsule_controls(origin: Store, max_bytes: Option<u64>) -> Store {
    if origin.cdn_binding().is_none() {
        return origin;
    }
    let mut cache = LocalCache::with_limits(default_cache_root(), max_bytes, max_bytes);
    if let Some(namespace) = origin.authorized_cache_namespace() {
        cache = cache.for_authorized_reads(namespace);
    }
    attach(origin, Arc::new(cache))
}

pub(super) fn attach(origin: Store, cache: Arc<LocalCache>) -> Store {
    origin.with_capsule_control_cache(Arc::new(LocalControls(cache)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_storage::{
        ReadAuthorization, StorageError, StoreLayout,
        cdn::{
            CdnAccess, CdnConsumer, CdnCredential, CdnCredentials, CdnProfile, CdnRoute,
            CdnTransport,
        },
    };
    use object_store::{memory::InMemory, path::Path};
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    struct Grant(AtomicBool);

    #[async_trait::async_trait]
    impl ReadAuthorization for Grant {
        async fn check(&self) -> crab_storage::Result<()> {
            if self.0.load(Ordering::Relaxed) {
                Ok(())
            } else {
                Err(StorageError::Forbidden {
                    path: "view expired".into(),
                })
            }
        }
    }

    #[async_trait::async_trait]
    impl CdnCredentials for Grant {
        async fn credential(
            &self,
            _: &Path,
            _: Option<u64>,
        ) -> crab_storage::Result<Option<CdnCredential>> {
            self.check().await?;
            Ok(Some(CdnCredential::cookie(1, "viewer=fixture".into())))
        }
    }

    #[tokio::test]
    async fn capsule_control_reopens_verified_edge_bytes_and_rechecks_the_view() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let grant = Arc::new(Grant(AtomicBool::new(true)));
        let dir = tempfile::tempdir().unwrap();
        let create_store = || {
            let target = [9; 32];
            let origin = Store::new(Arc::new(InMemory::new()))
                .with_target_identity(target)
                .with_read_authorization(grant.clone(), "org/repo")
                .unwrap()
                .with_cdn_route(
                    CdnRoute::new(
                        target,
                        "org/repo".into(),
                        None,
                        CdnAccess::Private(grant.clone()),
                        CdnTransport::loopback_fixture(address).unwrap(),
                        Duration::from_secs(6),
                    )
                    .for_consumer(CdnConsumer::Cli, CdnProfile::Direct),
                )
                .unwrap();
            super::super::CachingStore::new_with_local_cache(
                origin,
                super::super::CacheConfig::default(),
                Arc::new(LocalCache::new(dir.path().join("cache"))),
            )
            .unwrap()
            .cache_aware_storage()
        };
        let serve = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(socket);
            let mut request = String::new();
            while !request.ends_with("\r\n\r\n") {
                assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
            }
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("range: bytes=3-9\r\n")
            );
            stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-9/12\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncontrol").await.unwrap();
        });
        let path = StoreLayout::with_global_prefix((), "org/repo".into(), ".crab".into())
            .capsule_path(&"a".repeat(64));
        let verify = |body: Bytes| async move {
            assert_eq!(&body[..], b"control");
            Ok::<_, StorageError>(body)
        };
        assert_eq!(
            create_store()
                .read_verified(&path, 12, Some(3..10), verify)
                .await
                .unwrap(),
            "control"
        );
        tokio::time::timeout(Duration::from_secs(3), serve)
            .await
            .unwrap()
            .unwrap();
        // A second reader opens the disk entry after the one-request edge closes.
        assert_eq!(
            create_store()
                .read_verified(&path, 12, Some(3..10), verify)
                .await
                .unwrap(),
            "control"
        );
        grant.0.store(false, Ordering::Relaxed);
        assert!(matches!(
            create_store()
                .read_verified(&path, 12, Some(3..10), verify)
                .await,
            Err(StorageError::ReadRejected { .. })
        ));
    }
}
