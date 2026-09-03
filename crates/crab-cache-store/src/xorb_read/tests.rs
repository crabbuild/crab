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
    XorbReadKey::new(MerkleHash::from([id, 0, 0, 0]), &[(0, 1)], false).unwrap()
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
    let key = XorbReadKey::new(MerkleHash::default(), &ranges, false).unwrap();
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
