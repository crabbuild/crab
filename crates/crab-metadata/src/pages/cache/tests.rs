use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_admission_shares_page_and_materialization_limits() {
    for mode in 0..3 {
        let work = PageWork::new(64, 0);
        let ready = Arc::new(tokio::sync::Barrier::new(33));
        let mut tasks = Vec::new();
        for layer in 0..32 {
            let mut work = work.clone();
            let ready = ready.clone();
            tasks.push(tokio::spawn(async move {
                ready.wait().await;
                match mode {
                    0 => work.admit(0, 0, 16),
                    1 => work.admit(layer, 0, 16),
                    _ => work.admit_materialized(64),
                }
                .is_ok()
            }));
        }
        ready.wait().await;
        let mut admitted = 0;
        for task in tasks {
            admitted += usize::from(task.await.unwrap());
        }
        let expected = [(32, 16), (4, 64), (4, 0)][mode];
        assert_eq!((admitted, work.bytes().unwrap()), expected);
    }
}
use std::sync::atomic::{AtomicU64, Ordering};

#[tokio::test]
async fn primed_routing_keeps_cold_and_warm_byte_admission() {
    let mut writer = Writer::with_targets(4096, 4096);
    let rows = (0_u64..1000)
        .map(|index| {
            let mut key = vec![0; 20];
            key[12..].copy_from_slice(&index.to_be_bytes());
            (key, vec![])
        })
        .collect::<Rows>();
    let key = rows[333].0.clone();
    let root = writer.tree(Kind::VisibilityChanges, rows).unwrap();
    let body = bytes::Bytes::from(writer.bytes);
    let region = 0..body.len() as u64;
    let reads = Arc::new(AtomicU64::new(0));
    let observed = reads.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
    let path = object_store::path::Path::from("primed-routing");
    store.create_strict(&path, body.clone()).await.unwrap();
    let cache = PageCache::default();
    let inline = &body[root.offset as usize..root.offset as usize + root.bytes as usize];
    cache
        .prime(
            &path,
            0,
            region.clone(),
            &root,
            Kind::VisibilityChanges,
            inline,
        )
        .unwrap();
    let mut tight = PageWork::new(u64::from(root.bytes) - 1, 0);
    assert!(
        cache
            .lookup(
                &store,
                &path,
                0,
                region.clone(),
                &root,
                Kind::VisibilityChanges,
                std::slice::from_ref(&key),
                &mut tight
            )
            .await
            .is_err()
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    let mut observations = Vec::new();
    for _ in 0..2 {
        reads.store(0, Ordering::SeqCst);
        let mut work = PageWork::new(region.end, 0);
        assert_eq!(
            cache
                .lookup(
                    &store,
                    &path,
                    0,
                    region.clone(),
                    &root,
                    Kind::VisibilityChanges,
                    std::slice::from_ref(&key),
                    &mut work
                )
                .await
                .unwrap(),
            vec![Some(vec![])]
        );
        observations.push((reads.load(Ordering::SeqCst), work.bytes().unwrap()));
    }
    assert_eq!(observations[0].0, 1);
    assert_eq!(observations[1], (0, observations[0].1));
    assert!(observations[0].1 > u64::from(root.bytes));
}

#[test]
fn primed_pages_cannot_replace_a_captured_routing_identity() {
    let path = object_store::path::Path::from("immutable-routing");
    let cache = PageCache::default();
    for (value, accepted) in [(1, true), (2, false)] {
        let mut writer = Writer::default();
        let root = writer
            .tree(Kind::VisibilityChanges, vec![(vec![value; 20], vec![])])
            .unwrap();
        let region = 0..writer.bytes.len() as u64;
        assert_eq!(
            cache
                .prime(
                    &path,
                    0,
                    region,
                    &root,
                    Kind::VisibilityChanges,
                    &writer.bytes
                )
                .is_ok(),
            accepted
        );
    }
}

#[tokio::test]
async fn cold_bulk_ranges_use_one_transfer_and_preserve_point_proofs() {
    let mut writer = Writer::default();
    let root = writer
        .tree(Kind::VisibilityChanges, vec![(vec![1; 20], vec![])])
        .unwrap();
    writer
        .bytes
        .resize(CACHE_BYTES + READ_AHEAD_BYTES as usize, 0);
    let expected = bytes::Bytes::from(writer.bytes);
    let reads = Arc::new(AtomicU64::new(0));
    let observed = reads.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
    let path = object_store::path::Path::from("proof");
    store.create_strict(&path, expected.clone()).await.unwrap();
    let region = 0..expected.len() as u64;
    let cache = PageCache::for_region(region.clone());
    let point = cache
        .fetch(
            &store,
            &path,
            0,
            region.clone(),
            &root,
            Kind::VisibilityChanges,
        )
        .await
        .unwrap();
    reads.store(0, Ordering::SeqCst);
    let bulk = cache
        .read_range(&store, &path, region.clone(), u64::MAX)
        .await
        .unwrap();
    let bulk_reads = reads.swap(0, Ordering::SeqCst);
    let retained = cache
        .fetch(&store, &path, 0, region, &root, Kind::VisibilityChanges)
        .await
        .unwrap();
    assert_eq!(
        (
            bulk == expected,
            bulk_reads,
            reads.load(Ordering::SeqCst),
            Arc::ptr_eq(&point, &retained)
        ),
        (true, 1, 0, true),
    );
}

#[tokio::test]
async fn fully_warm_bulk_ranges_reuse_blocks_without_a_transfer() {
    let reads = Arc::new(AtomicU64::new(0));
    let observed = reads.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
    let path = object_store::path::Path::from("proof");
    let expected = bytes::Bytes::from(vec![23; 3 * READ_AHEAD_BYTES as usize]);
    store.create_strict(&path, expected.clone()).await.unwrap();
    let region = 0..expected.len() as u64;
    let cache = PageCache::for_region(region.clone());
    for offset in (0..region.end).step_by(READ_AHEAD_BYTES as usize) {
        cache
            .read_range(&store, &path, offset..offset + 1, u64::MAX)
            .await
            .unwrap();
    }
    reads.store(0, Ordering::SeqCst);
    let bulk = cache
        .read_range(&store, &path, region, u64::MAX)
        .await
        .unwrap();
    assert_eq!((bulk, reads.load(Ordering::SeqCst)), (expected, 0));
}

#[tokio::test]
async fn oversized_point_pages_keep_admission_and_the_decoded_cache() {
    let key = vec![1; 4];
    let value = vec![7; READ_AHEAD_BYTES as usize + 1];
    let mut writer = Writer::default();
    let root = writer
        .tree(Kind::Records, vec![(key.clone(), value.clone())])
        .unwrap();
    let reads = Arc::new(AtomicU64::new(0));
    let observed = reads.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
    let path = object_store::path::Path::from("proof");
    let region = 0..writer.bytes.len() as u64;
    store
        .create_strict(&path, writer.bytes.into())
        .await
        .unwrap();
    let cache = PageCache::for_region(region.clone());
    let mut observations = Vec::new();
    for _ in 0..2 {
        reads.store(0, Ordering::SeqCst);
        let mut work = PageWork::new(region.end, 0);
        let result = cache
            .lookup(
                &store,
                &path,
                0,
                region.clone(),
                &root,
                Kind::Records,
                std::slice::from_ref(&key),
                &mut work,
            )
            .await
            .unwrap();
        assert_eq!(result, [Some(value.clone())]);
        observations.push((reads.load(Ordering::SeqCst), work.bytes().unwrap()));
    }
    assert_eq!(observations, [(1, region.end), (0, region.end)]);
}

#[tokio::test]
async fn bulk_ranges_preserve_tight_budgets_with_cold_and_warm_blocks() {
    let bytes = Arc::new(AtomicU64::new(0));
    let observed = bytes.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_byte_observer(Arc::new(move |count| {
            observed.fetch_add(count, Ordering::SeqCst);
        }));
    let path = object_store::path::Path::from("proof");
    let body = bytes::Bytes::from((0u8..128).collect::<Vec<_>>());
    store.create_strict(&path, body.clone()).await.unwrap();
    let cache = PageCache::for_region(0..128);
    for warm in [false, true] {
        if warm {
            cache.read_range(&store, &path, 0..128, 128).await.unwrap();
        }
        bytes.store(0, Ordering::SeqCst);
        let actual = cache.read_range(&store, &path, 40..50, 10).await.unwrap();
        assert_eq!(
            (actual, bytes.load(Ordering::SeqCst)),
            (body.slice(40..50), 10)
        );
        bytes.store(0, Ordering::SeqCst);
        let rejected = cache.read_range(&store, &path, 40..50, 9).await.is_err();
        assert_eq!((rejected, bytes.load(Ordering::SeqCst)), (true, 0));
    }
}

#[tokio::test]
async fn bulk_forks_reuse_warm_blocks_without_retaining_scan_pages() {
    let mut writer = Writer::default();
    let warm = writer
        .tree(Kind::VisibilityChanges, vec![(vec![1; 20], vec![])])
        .unwrap();
    // Separate proof blocks keep read-ahead from warming the scanned page.
    writer.bytes.resize(READ_AHEAD_BYTES as usize, 0);
    let expected = vec![(vec![2; 20], vec![])];
    let cold = writer
        .tree(Kind::VisibilityChanges, expected.clone())
        .unwrap();
    let bytes = Arc::new(AtomicU64::new(0));
    let observed = bytes.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_byte_observer(Arc::new(move |count| {
            observed.fetch_add(count, Ordering::SeqCst);
        }));
    let path = object_store::path::Path::from("proof");
    let region = 0..writer.bytes.len() as u64;
    store
        .create_strict(&path, writer.bytes.into())
        .await
        .unwrap();
    let cache = PageCache::for_region(region.clone());
    cache
        .fetch(
            &store,
            &path,
            0,
            region.clone(),
            &warm,
            Kind::VisibilityChanges,
        )
        .await
        .unwrap();
    bytes.store(0, Ordering::SeqCst);
    let fork = cache.fork().unwrap();
    fork.fetch(
        &store,
        &path,
        0,
        region.clone(),
        &warm,
        Kind::VisibilityChanges,
    )
    .await
    .unwrap();
    let fork_warm_bytes = bytes.swap(0, Ordering::SeqCst);
    let scanned = fork
        .scan(
            &store,
            &path,
            0,
            region.clone(),
            &cold,
            Kind::VisibilityChanges,
            &mut PageWork::new(u64::MAX, 0),
        )
        .await
        .unwrap();
    drop(fork);
    bytes.store(0, Ordering::SeqCst);
    cache
        .fetch(
            &store,
            &path,
            0,
            region.clone(),
            &warm,
            Kind::VisibilityChanges,
        )
        .await
        .unwrap();
    let parent_warm_bytes = bytes.swap(0, Ordering::SeqCst);
    cache
        .fetch(&store, &path, 0, region, &cold, Kind::VisibilityChanges)
        .await
        .unwrap();
    assert_eq!(
        (
            scanned,
            fork_warm_bytes,
            parent_warm_bytes,
            bytes.load(Ordering::SeqCst) > 0
        ),
        (expected, 0, 0, true),
    );
}

#[tokio::test]
async fn coalesced_reads_stay_inside_proof_region_and_charge_cold_and_warm_work() {
    let mut writer = Writer::default();
    let root = writer
        .tree(Kind::VisibilityChanges, vec![(vec![1; 20], vec![])])
        .unwrap();
    let bytes = Arc::new(AtomicU64::new(0));
    let observed = bytes.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_byte_observer(Arc::new(move |count| {
            observed.fetch_add(count, Ordering::SeqCst);
        }));
    let path = object_store::path::Path::from("proof");
    let region = 128..128 + writer.bytes.len() as u64;
    let mut body = vec![0; 128];
    body.extend_from_slice(&writer.bytes);
    body.extend_from_slice(&[0; 128]);
    store.create_strict(&path, body.into()).await.unwrap();
    let cache = PageCache::for_region(region.clone());
    let mut work = PageWork::new(u64::MAX, 0);
    cache
        .lookup(
            &store,
            &path,
            0,
            region.clone(),
            &root,
            Kind::VisibilityChanges,
            &[vec![1; 20]],
            &mut work,
        )
        .await
        .unwrap();
    let cold = bytes.swap(0, Ordering::SeqCst);
    let mut limited = PageWork::new(cold - 1, 0);
    let rejected = cache
        .lookup(
            &store,
            &path,
            0,
            region.clone(),
            &root,
            Kind::VisibilityChanges,
            &[vec![1; 20]],
            &mut limited,
        )
        .await
        .is_err();
    assert_eq!(
        (
            cold,
            work.bytes().unwrap(),
            rejected,
            bytes.load(Ordering::SeqCst)
        ),
        (region.end - region.start, cold, true, 0)
    );
}

#[tokio::test]
async fn cancelled_queued_read_does_not_keep_the_cache_lane_locked() {
    use futures_util::FutureExt;
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let path = object_store::path::Path::from("proof");
    let expected = bytes::Bytes::from_static(b"block");
    store.create_strict(&path, expected.clone()).await.unwrap();
    let range = 0..expected.len() as u64;
    let cache = PageCache::for_region(range.clone());
    let held = cache.block_reads[0].lock().await;
    // Dropping a queued read must unregister its waiter; otherwise a
    // cancelled query can strand later cold reads sharing this lane.
    let queued = cache
        .read_block(&store, &path, range.clone())
        .now_or_never()
        .is_none();
    drop(held);
    let actual = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        cache.read_block(&store, &path, range),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!((queued, actual), (true, expected));
}

#[tokio::test]
async fn prefetched_pages_still_require_their_own_authentication() {
    let mut writer = Writer::default();
    let first = writer
        .tree(Kind::VisibilityChanges, vec![(vec![1; 20], vec![])])
        .unwrap();
    let second = writer
        .tree(Kind::VisibilityChanges, vec![(vec![2; 20], vec![])])
        .unwrap();
    writer.bytes[second.offset as usize] ^= 1;
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let path = object_store::path::Path::from("proof");
    let region = 0..writer.bytes.len() as u64;
    store
        .create_strict(&path, writer.bytes.into())
        .await
        .unwrap();
    let cache = PageCache::for_region(region.clone());
    cache
        .fetch(
            &store,
            &path,
            0,
            region.clone(),
            &first,
            Kind::VisibilityChanges,
        )
        .await
        .unwrap();
    assert!(
        cache
            .fetch(&store, &path, 0, region, &second, Kind::VisibilityChanges)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn proof_blocks_and_decoded_pages_share_one_cache_bound() {
    let region = 0..256 * READ_AHEAD_BYTES;
    let cache = PageCache::for_region(region.clone());
    for offset in 1..256 {
        cache
            .cache_block(
                offset * READ_AHEAD_BYTES,
                vec![0; READ_AHEAD_BYTES as usize].into(),
            )
            .unwrap();
    }
    let mut writer = Writer::default();
    let root = writer
        .tree(Kind::VisibilityChanges, vec![(vec![1; 20], vec![])])
        .unwrap();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let path = object_store::path::Path::from("proof");
    writer.bytes.resize(region.end as usize, 0);
    store
        .create_strict(&path, writer.bytes.into())
        .await
        .unwrap();
    let page = cache
        .fetch(&store, &path, 0, region, &root, Kind::VisibilityChanges)
        .await
        .unwrap();
    let mut state = cache.state().unwrap();
    state.evict_for(MAX_BYTES);
    assert!(
        state.bytes + MAX_BYTES <= CACHE_BYTES
            && state.pages.len() + state.blocks.len() <= CACHE_PAGES
            && Arc::ptr_eq(&state.pages.get(&(0, root.offset)).unwrap().2, &page)
    );
}
