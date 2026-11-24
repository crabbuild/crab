use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use bytes::Bytes;
use crab_lfs::{LfsError, LfsObjectStore};
use crab_storage::{
    Store,
    cdn::{CdnAccess, CdnConsumer, CdnProfile, CdnRoute, CdnTransport},
};
use futures_util::TryStreamExt;
use object_store::memory::InMemory;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

struct Capture {
    edge: Arc<AtomicUsize>,
    local: Arc<AtomicUsize>,
    key: String,
}

impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "crab_storage::cdn::transfer"
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        #[derive(Default)]
        struct Payload {
            verified: bool,
            source: String,
            key: String,
        }
        impl Visit for Payload {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.verified = format!("{value:?}") == "CDN verified LFS payload";
                }
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                match field.name() {
                    "source" => self.source = value.to_owned(),
                    "lfs_key_sha256" => self.key = value.to_owned(),
                    _ => {}
                }
            }
        }
        let mut payload = Payload::default();
        event.record(&mut payload);
        if payload.verified {
            assert_eq!(payload.key, self.key);
            match payload.source.as_str() {
                "edge" => self.edge.fetch_add(1, Ordering::Relaxed),
                "local" => self.local.fetch_add(1, Ordering::Relaxed),
                source => panic!("unexpected verified LFS payload source {source}"),
            };
        }
    }
}

#[tokio::test]
async fn observes_only_edge_payloads_that_pass_full_verification() {
    // Give this binary a single subscriber before any edge callsites register.
    let accepted = Arc::new(AtomicUsize::new(0));
    let local = Arc::new(AtomicUsize::new(0));
    let oid = Sha256::digest(b"verified").into();
    let key = LfsObjectStore::object_path_for_prefix("stream", &oid);
    tracing::subscriber::set_global_default(Capture {
        edge: Arc::clone(&accepted),
        local: Arc::clone(&local),
        key: format!("{:x}", Sha256::digest(key.as_ref().as_bytes())),
    })
    .unwrap();
    let scratch = tempfile::tempdir().unwrap();
    for body in [b"corrupt!", b"verified"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let transport = CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
                }
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("\r\nrange: bytes=0-7\r\n")
                );
                stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-7/8\r\nContent-Length: 8\r\nConnection: close\r\n\r\n").await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });
        let target = [4; 32];
        let store = Store::new(Arc::new(InMemory::new()))
            .with_target_identity(target)
            .with_cdn_route(
                CdnRoute::new(
                    target,
                    "stream".into(),
                    None,
                    CdnAccess::Public,
                    transport,
                    Duration::from_secs(10),
                )
                .for_consumer(CdnConsumer::Cli, CdnProfile::Direct),
            )
            .unwrap();
        let lfs = LfsObjectStore::new(store, "stream");
        lfs.put(&oid, Bytes::from_static(b"verified"))
            .await
            .unwrap();
        // The file can recover at origin. Its success must not credit the
        // preceding corrupt edge response.
        lfs.download_to_file(&oid, 8, &scratch.path().join("object"))
            .await
            .unwrap();
        let (_, stream) = lfs.stream_content(&oid, 8, None, None).await.unwrap();
        let result = stream.try_collect::<Vec<_>>().await;
        server.await.unwrap();
        if body == b"verified" {
            assert_eq!(result.unwrap().concat(), body);
            assert_eq!(accepted.load(Ordering::Relaxed), 2);
            let cached = std::fs::read(scratch.path().join("object")).unwrap();
            assert_eq!(Sha256::digest(&cached).as_slice(), oid);
            lfs.store()
                .observe_verified_lfs_cache_hit(&key, cached.len() as u64);
            assert_eq!(local.load(Ordering::Relaxed), 1);
        } else {
            assert!(matches!(result, Err(LfsError::ObjectCorrupt { .. })));
            assert_eq!(accepted.load(Ordering::Relaxed), 0);
        }
    }
    let native = Store::new(Arc::new(InMemory::new()));
    native.observe_verified_lfs_cache_hit(&key, 8);
    assert_eq!(local.load(Ordering::Relaxed), 1);
}
