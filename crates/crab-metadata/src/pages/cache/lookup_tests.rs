use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    store: Store,
    path: object_store::path::Path,
    source: std::ops::Range<u64>,
    body: bytes::Bytes,
    root: PageRef,
    children: Vec<PageRef>,
    keys: Vec<Vec<u8>>,
    reads: Arc<AtomicU64>,
    fetched: Arc<AtomicU64>,
    largest: Arc<AtomicU64>,
}

async fn fixture(rows: u64, offset: usize) -> Fixture {
    let mut writer = Writer::with_targets(512, 4096);
    let rows = (0..rows)
        .map(|index| {
            let mut key = vec![0; 20];
            key[12..].copy_from_slice(&index.to_be_bytes());
            (key, vec![])
        })
        .collect::<Rows>();
    let keys = rows.iter().map(|row| row.0.clone()).collect();
    let root = writer.tree(Kind::VisibilityChanges, rows).unwrap();
    let body = bytes::Bytes::from(writer.bytes);
    let encoded_root = page_bytes(&body, &root, "fixture").unwrap();
    let Page::Branch(children) = decode(
        encoded_root,
        &root,
        Kind::VisibilityChanges,
        body.len() as u64,
        "fixture",
    )
    .unwrap() else {
        panic!("fixture must have a directory")
    };
    let reads = Arc::new(AtomicU64::new(0));
    let fetched = Arc::new(AtomicU64::new(0));
    let largest = Arc::new(AtomicU64::new(0));
    let read_counter = reads.clone();
    let byte_counter = fetched.clone();
    let largest_counter = largest.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_request_observer(Arc::new(move |_| {
            read_counter.fetch_add(1, Ordering::SeqCst);
        }))
        .with_read_byte_observer(Arc::new(move |bytes| {
            byte_counter.fetch_add(bytes, Ordering::SeqCst);
            largest_counter.fetch_max(bytes, Ordering::SeqCst);
        }));
    let path = object_store::path::Path::from("batched-pages");
    let mut stored = vec![7; offset];
    stored.extend_from_slice(&body);
    store.create_strict(&path, stored.into()).await.unwrap();
    reads.store(0, Ordering::SeqCst);
    let source = offset as u64..offset as u64 + body.len() as u64;
    Fixture {
        store,
        path,
        source,
        body,
        root,
        children,
        keys,
        reads,
        fetched,
        largest,
    }
}

fn prime(f: &Fixture, cache: &PageCache, reference: &PageRef) {
    let body = page_bytes(&f.body, reference, "fixture").unwrap();
    cache
        .prime(
            &f.path,
            0,
            f.source.clone(),
            reference,
            Kind::VisibilityChanges,
            body,
        )
        .unwrap();
}

#[tokio::test]
async fn lookup_batches_adjacent_pages_without_extra_bytes() {
    for (offset, read_ahead) in [(0, false), (97, false), (97, true)] {
        let mut f = fixture(256, offset).await;
        assert_eq!(f.root.height, 1);
        f.keys.push(f.keys[0].clone());
        f.keys.push(vec![255; 20]);
        let cache = if read_ahead {
            PageCache::for_region(f.source.clone())
        } else {
            PageCache::default()
        };
        let mut work = PageWork::new(f.body.len() as u64, 0);
        let found = cache
            .lookup(
                &f.store,
                &f.path,
                0,
                f.source.clone(),
                &f.root,
                Kind::VisibilityChanges,
                &f.keys,
                &mut work,
            )
            .await
            .unwrap();
        let mut expected = vec![Some(vec![]); 257];
        expected.push(None);
        let expected_reads = if read_ahead { 1 } else { 2 };
        assert_eq!(
            (
                found,
                f.reads.load(Ordering::SeqCst),
                f.fetched.load(Ordering::SeqCst)
            ),
            (expected, expected_reads, f.body.len() as u64)
        );
    }
}

#[tokio::test]
async fn cached_pages_and_unselected_gaps_are_not_fetched() {
    let f = fixture(256, 0).await;
    let cache = PageCache::default();
    prime(&f, &cache, &f.root);
    prime(&f, &cache, &f.children[2]);
    let keys = [0, 2, 4].map(|i| f.children[i].first.clone());
    let expected_bytes = u64::from(f.children[0].bytes) + u64::from(f.children[4].bytes);
    for _ in 0..2 {
        let mut work = PageWork::new(f.body.len() as u64, 0);
        let found = cache
            .lookup(
                &f.store,
                &f.path,
                0,
                f.source.clone(),
                &f.root,
                Kind::VisibilityChanges,
                &keys,
                &mut work,
            )
            .await
            .unwrap();
        assert_eq!(
            (
                found,
                f.reads.load(Ordering::SeqCst),
                f.fetched.load(Ordering::SeqCst)
            ),
            (vec![Some(vec![]); 3], 2, expected_bytes)
        );
    }
}

#[tokio::test]
async fn each_coalesced_page_keeps_its_hash_proof() {
    let f = fixture(256, 0).await;
    let cache = PageCache::default();
    prime(&f, &cache, &f.root);
    let mut corrupt = f.body.to_vec();
    corrupt[f.children[1].offset as usize + HEADER] ^= 1;
    // Inject corrupt origin bytes below Store's immutable-write contract.
    f.store
        .inner()
        .put_opts(
            &f.path,
            bytes::Bytes::from(corrupt).into(),
            object_store::PutOptions::default(),
        )
        .await
        .unwrap();
    let keys = [f.children[0].first.clone(), f.children[1].first.clone()];
    let mut work = PageWork::new(f.body.len() as u64, 0);
    let result = cache
        .lookup(
            &f.store,
            &f.path,
            0,
            f.source.clone(),
            &f.root,
            Kind::VisibilityChanges,
            &keys,
            &mut work,
        )
        .await;
    assert_eq!(
        (
            matches!(result, Err(MetadataError::CorruptObject { .. })),
            f.reads.load(Ordering::SeqCst)
        ),
        (true, 1)
    );
}

#[tokio::test]
async fn lookup_admits_the_whole_wave_before_cold_or_warm_reads() {
    let f = fixture(256, 0).await;
    let keys = [f.children[0].first.clone(), f.children[1].first.clone()];
    let maximum =
        u64::from(f.root.bytes) + u64::from(f.children[0].bytes) + u64::from(f.children[1].bytes)
            - 1;
    for warm in [false, true] {
        let cache = PageCache::default();
        prime(&f, &cache, &f.root);
        if warm {
            prime(&f, &cache, &f.children[0]);
            prime(&f, &cache, &f.children[1]);
        }
        let mut work = PageWork::new(maximum, 0);
        let result = cache
            .lookup(
                &f.store,
                &f.path,
                0,
                f.source.clone(),
                &f.root,
                Kind::VisibilityChanges,
                &keys,
                &mut work,
            )
            .await;
        assert_eq!(
            (
                result.is_err(),
                f.reads.load(Ordering::SeqCst),
                f.fetched.load(Ordering::SeqCst)
            ),
            (true, 0, 0)
        );
    }
}

#[tokio::test]
async fn lookup_transfer_groups_are_bounded() {
    let f = fixture(10_000, 0).await;
    let cache = PageCache::default();
    let mut work = PageWork::new(f.body.len() as u64, 0);
    let found = cache
        .lookup(
            &f.store,
            &f.path,
            0,
            f.source.clone(),
            &f.root,
            Kind::VisibilityChanges,
            &f.keys,
            &mut work,
        )
        .await
        .unwrap();
    assert_eq!(
        (
            found,
            f.fetched.load(Ordering::SeqCst),
            f.largest.load(Ordering::SeqCst) <= READ_AHEAD_BYTES
        ),
        (vec![Some(vec![]); f.keys.len()], f.body.len() as u64, true)
    );
}
