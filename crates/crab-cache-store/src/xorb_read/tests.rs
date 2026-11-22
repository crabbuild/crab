use super::*;

#[tokio::test]
async fn delayed_decode_failure_preserves_a_healthy_refill() {
    use crate::CacheConfig;
    use crab_cache::{CacheCatalog, LocalCache};
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;
    use std::sync::Arc;

    let mut builder = XorbBuilder::new();
    builder
        .push(&Chunk::new(Bytes::from_static(b"data")), RunId(0))
        .unwrap();
    let xorb = builder.finalize().unwrap().pop().unwrap();
    let hash = xorb.hash;
    let temp = tempfile::tempdir().unwrap();
    let cache = Arc::new(LocalCache::new(temp.path().join("cache")));
    cache
        .put_read_xorb(&hash, xorb.bytes.clone())
        .await
        .unwrap();
    let local_path = cache
        .root()
        .join("xorbs")
        .join(&hash.hex()[..2])
        .join(hash.hex());
    let mut corrupt = xorb.bytes.to_vec();
    corrupt[0] ^= 0xff;
    std::fs::write(&local_path, corrupt).unwrap();
    // The local identity check succeeds; decoding/digest verification belongs
    // to the outer reader and can finish after another process publishes.
    let old = cache
        .get_read_xorb_if_present(&hash)
        .await
        .unwrap()
        .unwrap();
    let error = XorbParser::parse(old)
        .unwrap()
        .verify_payload_digest()
        .unwrap_err();
    cache
        .put_read_xorb(&hash, xorb.bytes.clone())
        .await
        .unwrap();
    let before = CacheCatalog::read_only_stats(cache.root()).unwrap();
    let origin = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let store =
        CachingStore::new_with_local_cache(origin, CacheConfig::default(), cache.clone()).unwrap();
    let path = Path::from(format!(".crab/xorbs/{}/{hash}", &hash.hex()[..2]));
    store
        .xorb_read_failed(
            &path,
            &hash,
            XorbSource::Local,
            CacheError::from(error).into(),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(local_path).unwrap(), xorb.bytes);
    assert_eq!(CacheCatalog::read_only_stats(cache.root()).unwrap(), before);
}

fn key(id: u64) -> XorbReadKey {
    XorbReadKey::new(
        MerkleHash::from([id, 0, 0, 0]),
        &[(0, 1)],
        XorbReadMode::Whole,
    )
    .unwrap()
}

#[test]
fn retained_slice_does_not_pin_the_complete_xorb() {
    let state = XorbReadState::new();
    let body = Bytes::from(vec![7; 8 * 1024 * 1024]);
    let value = (body.slice(1024..1027), vec![0, 3]);
    state.insert(key(1), &value);

    let cached = state.get(&key(1)).unwrap();
    assert_eq!(cached, value);
    assert_ne!(cached.0.as_ptr(), value.0.as_ptr());
    assert!(state.cache_guard().charged_bytes < 1024);
}

#[test]
fn charge_includes_both_range_keys_and_offsets() {
    let state = XorbReadState::new();
    let ranges = vec![(0, 1); 1024];
    let key = XorbReadKey::new(MerkleHash::default(), &ranges, XorbReadMode::Whole).unwrap();
    let value = (Bytes::from_static(b"abc"), vec![0, 3]);
    state.insert(key, &value);

    let cache = state.cache_guard();
    let key_bytes: usize = cache
        .entries
        .keys()
        .map(|key| key.ranges.capacity() * std::mem::size_of::<(u32, u32)>())
        .sum::<usize>()
        + cache
            .insertion_order
            .iter()
            .map(|key| key.ranges.capacity() * std::mem::size_of::<(u32, u32)>())
            .sum::<usize>();
    assert!(cache.charged_bytes >= key_bytes + 3 + 2 * std::mem::size_of::<u32>());
    assert_eq!(
        cache.charged_bytes,
        cache
            .entries
            .values()
            .map(|entry| entry.charge)
            .sum::<usize>()
    );
}

#[test]
fn byte_pressure_evicts_before_retaining_another_result() {
    let state = XorbReadState::new();
    let value = (
        Bytes::from(vec![1; XORB_READ_CACHE_MAX_BYTES / 4]),
        vec![0, (XORB_READ_CACHE_MAX_BYTES / 4) as u32],
    );
    for id in 0..4 {
        state.insert(key(id), &value);
        assert!(state.cache_guard().charged_bytes <= XORB_READ_CACHE_MAX_BYTES);
    }

    assert!(state.get(&key(0)).is_none());
    assert_eq!(state.get(&key(3)).unwrap(), value);
    assert_eq!(state.cache_guard().entries.len(), 3);
}

#[test]
fn small_results_cannot_grow_unbounded_bookkeeping() {
    let state = XorbReadState::new();
    let value = (Bytes::from_static(b"a"), vec![0, 1]);
    for id in 0..=XORB_READ_CACHE_MAX_ENTRIES {
        state.insert(key(id as u64), &value);
    }

    assert!(state.get(&key(0)).is_none());
    assert_eq!(
        state.get(&key(XORB_READ_CACHE_MAX_ENTRIES as u64)).unwrap(),
        value
    );
    let cache = state.cache_guard();
    assert_eq!(cache.entries.len(), XORB_READ_CACHE_MAX_ENTRIES);
    assert_eq!(cache.insertion_order.len(), cache.entries.len());
}

#[test]
fn oversized_key_capacity_is_not_retained() {
    let state = XorbReadState::new();
    let mut oversized = key(1);
    oversized.ranges =
        Vec::with_capacity(XORB_READ_CACHE_MAX_BYTES / std::mem::size_of::<(u32, u32)>());
    oversized.ranges.push((0, 1));
    state.insert(oversized, &(Bytes::from_static(b"a"), vec![0, 1]));

    assert_eq!(state.cache_guard().charged_bytes, 0);
    assert!(state.get(&key(1)).is_none());
}

#[test]
fn empty_results_do_not_occupy_cache_entries() {
    let state = XorbReadState::new();
    state.insert(key(1), &(Bytes::new(), vec![0]));
    let cache = state.cache_guard();
    assert!(cache.entries.is_empty());
    assert!(cache.insertion_order.is_empty());
    assert_eq!(cache.charged_bytes, 0);
}

#[test]
fn duplicate_insertion_keeps_the_verified_original() {
    let state = XorbReadState::new();
    let original = (Bytes::from_static(b"a"), vec![0, 1]);
    state.insert(key(1), &original);
    let before = state.cache_guard().charged_bytes;
    state.insert(key(1), &(Bytes::from_static(b"different"), vec![0, 9]));
    assert_eq!(state.get(&key(1)).unwrap(), original);
    assert_eq!(state.cache_guard().charged_bytes, before);
}

#[tokio::test]
async fn verified_xorb_chunks_report_their_delivery_source() {
    use crab_storage::cdn::{CdnAccess, CdnConsumer, CdnProfile, CdnRoute, CdnTransport};
    use crab_xet::xorb::{
        builder::{RunId, XorbBuilder},
        format::Chunk,
    };
    use object_store::{ObjectStoreExt, memory::InMemory};
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tracing::{
        Event, Metadata, Subscriber,
        field::{Field, Visit},
        span::{Attributes, Id, Record},
    };

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<(String, String)>>>);

    #[derive(Default)]
    struct Payload {
        source: String,
        fingerprint: Option<String>,
    }
    impl Visit for Payload {
        fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}

        fn record_str(&mut self, field: &Field, value: &str) {
            match field.name() {
                "source" => self.source = value.to_owned(),
                "xorb_key_sha256" => self.fingerprint = Some(value.to_owned()),
                _ => {}
            }
        }
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
            let mut payload = Payload::default();
            event.record(&mut payload);
            if let Some(fingerprint) = payload.fingerprint {
                self.0.lock().unwrap().push((payload.source, fingerprint));
            }
        }
    }

    let capture = Capture::default();
    let dispatch = tracing::Dispatch::new(capture.clone());
    let _guard = tracing::dispatcher::set_default(&dispatch);
    let expected = Bytes::from_static(b"cdn verified content");
    let mut builder = XorbBuilder::new();
    for chunk in [
        expected.clone(),
        Bytes::from_static(b"another chunk in the fixture"),
        Bytes::from_static(b"unrequested xorb chunk"),
    ] {
        builder.push(&Chunk::new(chunk), RunId(0)).unwrap();
    }
    let xorb = builder.finalize().unwrap().pop().unwrap();
    let path = crab_storage::canonical_global_content_path("xorbs", &xorb.hash.hex());
    let mut corrupt = xorb.bytes.to_vec();
    corrupt[0] ^= 1;
    let corrupt = Bytes::from(corrupt);

    for (mode, valid_edge) in [
        XorbReadMode::Whole,
        XorbReadMode::Selective,
        XorbReadMode::SelectiveCdn,
    ]
    .into_iter()
    .flat_map(|mode| [(mode, true), (mode, false)])
    {
        let (edge, native) = if valid_edge {
            (xorb.bytes.clone(), corrupt.clone())
        } else {
            (corrupt.clone(), xorb.bytes.clone())
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let transport = CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap();
        let server = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let mut socket = BufReader::new(socket);
                let mut range = None;
                let mut line = String::new();
                while socket.read_line(&mut line).await.unwrap() > 0 && line != "\r\n" {
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("range")
                    {
                        let (start, end) = value
                            .trim()
                            .strip_prefix("bytes=")
                            .unwrap()
                            .split_once('-')
                            .unwrap();
                        range = Some((
                            start.parse::<usize>().unwrap(),
                            end.parse::<usize>().unwrap(),
                        ));
                    }
                    line.clear();
                }
                let (status, body) = match range {
                    Some((start, end)) => (
                        format!(
                            "206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}",
                            edge.len()
                        ),
                        &edge[start..=end],
                    ),
                    None => ("200 OK".to_owned(), edge.as_ref()),
                };
                let headers = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket
                    .get_mut()
                    .write_all(headers.as_bytes())
                    .await
                    .unwrap();
                socket.get_mut().write_all(body).await.unwrap();
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let local = Arc::new(crab_cache::LocalCache::new(temp.path().join("cache")));
        let inner = Arc::new(InMemory::new());
        inner.put(&path, native.into()).await.unwrap();
        let target = [13; 32];
        let origin = crab_storage::Store::new(inner)
            .with_target_identity(target)
            .with_cdn_route(
                CdnRoute::new(
                    target,
                    "org/repo".into(),
                    Some(".crab".into()),
                    CdnAccess::Public,
                    transport,
                    Duration::from_secs(10),
                )
                .for_consumer(CdnConsumer::Sdk, CdnProfile::Direct),
            )
            .unwrap();
        let store =
            CachingStore::new_with_local_cache(origin, crate::CacheConfig::default(), local)
                .unwrap();
        if mode == XorbReadMode::Selective {
            assert_eq!(
                store
                    .xorb_chunk_metadata(&path, &xorb.hash)
                    .await
                    .unwrap()
                    .len(),
                3
            );
            assert!(capture.0.lock().unwrap().is_empty());
        }
        let range = [(0, 1)];
        for _ in 0..2 {
            let (bytes, _) = store
                .get_xorb_chunks_with_mode(&path, &xorb.hash, &range, mode)
                .await
                .unwrap();
            assert_eq!(bytes, expected);
        }
        let payloads = std::mem::take(&mut *capture.0.lock().unwrap());
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0].0, if valid_edge { "edge" } else { "origin" });
        assert_eq!(payloads[1].0, "local");
        assert_eq!(payloads[0].1, payloads[1].1);
        assert!(
            payloads
                .iter()
                .all(|(_, hash)| hash.len() == 64
                    && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        );
        server.abort();
    }
}
