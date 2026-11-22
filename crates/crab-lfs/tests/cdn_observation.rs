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

struct Capture(Arc<AtomicUsize>);

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
        struct Verified(bool);
        impl Visit for Verified {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}") == "CDN verified LFS payload";
                }
            }
        }
        let mut verified = Verified(false);
        event.record(&mut verified);
        if verified.0 {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[tokio::test]
async fn observes_only_edge_payloads_that_pass_full_verification() {
    // Give this binary a single subscriber before any edge callsites register.
    let accepted = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(Capture(Arc::clone(&accepted))).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let oid = Sha256::digest(b"verified").into();
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
        } else {
            assert!(matches!(result, Err(LfsError::ObjectCorrupt { .. })));
            assert_eq!(accepted.load(Ordering::Relaxed), 0);
        }
    }
}
