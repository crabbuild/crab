use super::*;
use crate::metadata::manifest::{Manifest, create_manifest};
use crate::storage::StoreLayout;
use crate::storage::store::Store;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
    PointerCatalog, ShardCatalogEntry,
};
use object_store::memory::InMemory;
use tempfile::TempDir;

fn setup() -> (StoreLayout, Arc<LocalCache>, TempDir) {
    let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
    let store = Store::new(inner);
    let router = StoreLayout::new(store, "repo".to_string());
    let dir = TempDir::new().unwrap();
    // Let the cache create its private root; TempDir inherits the process umask.
    let cache = Arc::new(LocalCache::new(dir.path().join("cache")));
    (router, cache, dir)
}

async fn publish_v2_shard(router: &StoreLayout, shard_data: &'static [u8]) -> MerkleHash {
    let shard_hash = compute_data_hash(shard_data);
    router
        .store()
        .put(
            &router.shard_path(&shard_hash),
            Bytes::from_static(shard_data),
        )
        .await
        .unwrap();
    let layout = crab_storage::StoreLayout::with_global_prefix(
        router.store().as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let base =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let view = crab_read::capsule_protocol::open_ref_view_from_root(&layout, base.clone())
        .await
        .unwrap();
    let old_oid = view.refs().get("refs/heads/main").cloned();
    let new_oid = shard_hash.hex()[..40].to_owned();
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_shard(
            shard_hash.hex(),
            ShardCatalogEntry::new(shard_data.len() as u64, Vec::new()),
        )
        .unwrap();
    let transaction = CapsuleTransaction::new(
        base.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            old_oid,
            Some(new_oid),
            None,
        )],
    )
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        Vec::new(),
        vec![CapsuleSection::new(
            CapsuleSectionKind::CatalogDelta,
            catalog.encode_delta().unwrap(),
        )],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(&layout, base, &transaction, &capsule)
        .await
        .unwrap();
    shard_hash
}

#[tokio::test]
async fn sync_with_empty_shard_list() {
    let (router, cache, _dir) = setup();
    let mut sync = ShardSynchronizer::new(router, cache, None);
    let mut idx = ChunkIndex::new();
    let shard_list = [];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
    assert_eq!(stats.shards_downloaded, 0);
    assert_eq!(stats.shards_skipped, 0);
    assert_eq!(stats.shards_failed, 0);
}

#[tokio::test]
async fn sync_skips_already_installed_shards() {
    let (router, cache, _dir) = setup();
    let mut sync = ShardSynchronizer::new(router, cache, None);
    let mut idx = ChunkIndex::new();

    let hash = MerkleHash::from([1u64, 1, 1, 1]);
    idx.install_shard(hash, &[]);

    let shard_list = [hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
    assert_eq!(stats.shards_skipped, 1);
    assert_eq!(stats.shards_downloaded, 0);
}

#[tokio::test]
async fn sync_handles_invalid_hash_gracefully() {
    let (router, cache, _dir) = setup();
    let mut sync = ShardSynchronizer::new(router, cache, None);
    let mut idx = ChunkIndex::new();

    let shard_list = ["not-a-valid-hex".to_string()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
    assert_eq!(stats.shards_failed, 1);
}

#[tokio::test]
async fn sync_handles_missing_shard_race_window() {
    let (router, cache, _dir) = setup();
    let mut sync = ShardSynchronizer::new(router, cache, None);
    let mut idx = ChunkIndex::new();

    // Shard listed but not uploaded — 404 (race window).
    let hash = MerkleHash::from([42u64, 42, 42, 42]);
    let shard_list = [hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
    assert_eq!(stats.shards_failed, 1);
    assert_eq!(stats.shards_downloaded, 0);
}

#[tokio::test]
async fn sync_downloads_and_installs_shard() {
    let (router, cache, _dir) = setup();

    let shard_data = b"fake shard data for testing";
    let hash = compute_data_hash(shard_data);
    let hex = hash.hex();
    // Upload to the global shard path that StoreLayout produces.
    let obj_path = router.shard_path(&hash);
    router
        .store()
        .put(&obj_path, Bytes::from(shard_data.as_slice()))
        .await
        .unwrap();

    let mut sync = ShardSynchronizer::new(router, cache.clone(), None);
    let mut idx = ChunkIndex::new();

    let shard_list = [hex.clone()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
    assert_eq!(stats.shards_downloaded, 1);
    assert!(idx.has_shard(&hash));
    assert!(cache.contains(&CacheKey::Shard(hash)).await);
}

#[tokio::test]
async fn persistent_receipts_skip_only_installed_shards() {
    let (router, cache, dir) = setup();
    let installed = MerkleHash::from([1_u64, 2, 3, 4]);
    let missing = MerkleHash::from([5_u64, 6, 7, 8]);
    let index_path = dir.path().join("receipts/chunk-index.sqlite");
    let persistent = Arc::new(PersistentChunkIndex::open_or_create(&index_path).unwrap());
    persistent.install_shard(installed, &[]).unwrap();
    let mut sync = ShardSynchronizer::new(router, cache, None).with_persistent_index(persistent);
    let mut index = ChunkIndex::new();
    let shards = [installed.hex(), missing.hex()];

    let stats = sync.sync(&mut index, &shards).await.unwrap();

    assert_eq!(stats.shards_skipped, 1);
    assert_eq!(stats.shards_failed, 1);
    assert_eq!(stats.shards_downloaded, 0);
}

#[tokio::test]
async fn post_fetch_sync_observes_new_shards_at_unchanged_root_generation() {
    let (router, cache, _dir) = setup();
    let first = publish_v2_shard(&router, b"first current shard").await;
    run_post_fetch_shard_sync(router.clone(), "repo-hash", cache.root(), None, false)
        .await
        .unwrap();
    let layout = crab_storage::StoreLayout::new(
        router.store().as_storage().clone(),
        router.repo_prefix().to_owned(),
    );
    let before = crab_metadata::capsule_protocol::load_root(&layout)
        .await
        .unwrap();
    let second = publish_v2_shard(&router, b"second current shard").await;
    let after = crab_metadata::capsule_protocol::load_root(&layout)
        .await
        .unwrap();
    assert_eq!(
        before.record().root().generation(),
        after.record().root().generation()
    );

    let stats = run_post_fetch_shard_sync(router.clone(), "repo-hash", cache.root(), None, false)
        .await
        .unwrap();

    assert_eq!(stats.shards_downloaded, 1);
    assert_eq!(stats.shards_skipped, 1);
    assert!(cache.contains(&CacheKey::Shard(first)).await);
    assert!(cache.contains(&CacheKey::Shard(second)).await);
}

#[tokio::test]
async fn post_fetch_sync_rejects_manifest_without_capsule_authority() {
    let (router, cache, _dir) = setup();
    crate::core::remote_layout::initialize(router.store(), &router)
        .await
        .unwrap();
    let manifest = Manifest::default_for_repo("refs/heads/main");
    create_manifest(router.store(), &router, &manifest)
        .await
        .unwrap();

    let result = run_post_fetch_shard_sync(router, "repo-hash", cache.root(), None, false).await;

    assert!(
        matches!(&result, Err(CrabError::NotFound { path }) if path.ends_with("/v2/root")),
        "expected missing Capsule authority, got {result:?}"
    );
    assert!(!cache.root().exists());
}

#[tokio::test]
async fn post_fetch_sync_reads_v2_pointer_catalog_without_manifest() {
    let (router, cache, _dir) = setup();
    let shard_hash = publish_v2_shard(&router, b"v2 pointer catalog shard").await;

    let stats = run_post_fetch_shard_sync(router, "repo-hash", cache.root(), None, false)
        .await
        .unwrap();

    assert_eq!(stats.shards_downloaded, 1);
    assert!(cache.contains(&CacheKey::Shard(shard_hash)).await);
}

#[tokio::test]
async fn corrupt_v2_root_does_not_fall_back_to_v1_shards() {
    let (router, _cache, _dir) = setup();
    let manifest = Manifest::default_for_repo("refs/heads/main");
    create_manifest(router.store(), &router, &manifest)
        .await
        .unwrap();
    let layout = crab_storage::StoreLayout::with_global_prefix(
        router.store().as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    router
        .store()
        .put(
            &layout.capsule_root_path(),
            Bytes::from_static(b"not a capsule root"),
        )
        .await
        .unwrap();

    assert!(matches!(
        load_published_shards(&router).await,
        Err(CrabError::CorruptObject { .. })
    ));
}

#[tokio::test]
async fn sync_detects_corrupt_download() {
    let (router, cache, _dir) = setup();

    let hash = MerkleHash::from([99u64, 99, 99, 99]);
    let obj_path = router.shard_path(&hash);
    router
        .store()
        .put(&obj_path, Bytes::from_static(b"wrong content"))
        .await
        .unwrap();

    let mut sync = ShardSynchronizer::new(router, cache, None);
    let mut idx = ChunkIndex::new();

    let shard_list = [hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
    assert_eq!(stats.shards_failed, 1);
    assert_eq!(stats.shards_downloaded, 0);
}

#[tokio::test]
async fn sync_emits_metrics() {
    let (router, cache, _dir) = setup();
    let metrics = Arc::new(Metrics::new());

    let shard_data = b"metrics test shard";
    let hash = compute_data_hash(shard_data);
    let hex = hash.hex();
    let obj_path = router.shard_path(&hash);
    router
        .store()
        .put(&obj_path, Bytes::from(shard_data.as_slice()))
        .await
        .unwrap();

    let mut sync = ShardSynchronizer::new(router, cache, Some(metrics.clone()));
    let mut idx = ChunkIndex::new();

    let shard_list = [hex];
    sync.sync(&mut idx, &shard_list).await.unwrap();

    let snap = metrics.snapshot();
    assert_eq!(snap.chunk_index_shards_installed, 1);
}

/// Build a canonical v1 shard with a bloom filter.
/// Returns `(shard_bytes, shard_hash)`.
fn build_bloom_shard(chunk_hashes: &[MerkleHash]) -> (Vec<u8>, MerkleHash) {
    use crab_xet::shard::ShardWriter;

    let w = ShardWriter::new();
    w.finalize_with_bloom(&[], chunk_hashes).unwrap()
}

/// Build a canonical v1 shard without the optional bloom.
fn build_plain_shard() -> (Vec<u8>, MerkleHash) {
    use crab_xet::shard::ShardWriter;

    let w = ShardWriter::new();
    w.finalize().unwrap()
}

fn build_shard_with_one_chunk() -> (
    Vec<u8>,
    MerkleHash,
    MerkleHash,
    crab_xet::xorb::format::XorbRef,
) {
    use crab_xet::shard::ShardWriter;
    use crab_xet::shard::{MDBXorbInfo, XorbChunkSequenceEntry, XorbChunkSequenceHeader};

    let xorb_hash = MerkleHash::from([7000, 7000, 7000, 7000]);
    let chunk_hash = MerkleHash::from([7001, 7001, 7001, 7001]);
    let chunk_size = 1024_u32;
    let xorb = Arc::new(MDBXorbInfo {
        metadata: XorbChunkSequenceHeader::new(xorb_hash, 1, chunk_size as usize),
        chunks: vec![XorbChunkSequenceEntry::new(chunk_hash, chunk_size, 0)],
    });
    let mut writer = ShardWriter::new();
    writer.add_xorb(xorb).unwrap();
    let (bytes, shard_hash) = writer.finalize().unwrap();
    (
        bytes,
        shard_hash,
        chunk_hash,
        crab_xet::xorb::format::XorbRef {
            xorb_hash,
            chunk_index: 0,
            uncompressed_size: chunk_size,
        },
    )
}

#[tokio::test]
async fn bloom_filter_skips_shard_when_no_chunk_matches() {
    let (router, cache, _dir) = setup();

    // Build a bloom shard with 500 chunk hashes to create a large bloom
    // filter where false positives for unrelated hashes are negligible.
    let shard_chunks: Vec<MerkleHash> = (10_000..10_500)
        .map(|i: u64| {
            MerkleHash::from([
                i,
                i.wrapping_mul(31),
                i.wrapping_mul(97),
                i.wrapping_mul(127),
            ])
        })
        .collect();
    let (shard_bytes, shard_hash) = build_bloom_shard(&shard_chunks);

    let obj_path = router.shard_path(&shard_hash);
    router
        .store()
        .put(&obj_path, Bytes::from(shard_bytes))
        .await
        .unwrap();

    // Single query hash from a completely different range.
    let query: HashSet<MerkleHash> = [MerkleHash::from([
        99_999u64,
        99_999u64.wrapping_mul(31),
        99_999u64.wrapping_mul(97),
        99_999u64.wrapping_mul(127),
    ])]
    .into_iter()
    .collect();

    let mut sync = ShardSynchronizer::new(router, cache, None).with_query_hashes(query);
    let mut idx = ChunkIndex::new();

    let shard_list = [shard_hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

    assert_eq!(stats.shards_bloom_skipped, 1);
    assert_eq!(stats.shards_downloaded, 0);
    assert!(!idx.has_shard(&shard_hash));
}

#[tokio::test]
async fn bloom_filter_downloads_shard_when_chunk_may_match() {
    let (router, cache, _dir) = setup();

    // Build a shard whose bloom contains 100 chunk hashes.
    let shard_chunks: Vec<MerkleHash> = (1000..1100)
        .map(|i: u64| {
            MerkleHash::from([
                i,
                i.wrapping_mul(31),
                i.wrapping_mul(97),
                i.wrapping_mul(127),
            ])
        })
        .collect();
    let (shard_bytes, shard_hash) = build_bloom_shard(&shard_chunks);

    let obj_path = router.shard_path(&shard_hash);
    router
        .store()
        .put(&obj_path, Bytes::from(shard_bytes))
        .await
        .unwrap();

    // Query includes hash 1050 which IS in the shard's bloom.
    let query: HashSet<MerkleHash> = [MerkleHash::from([
        1050u64,
        1050u64.wrapping_mul(31),
        1050u64.wrapping_mul(97),
        1050u64.wrapping_mul(127),
    ])]
    .into_iter()
    .collect();

    let mut sync = ShardSynchronizer::new(router, cache, None).with_query_hashes(query);
    let mut idx = ChunkIndex::new();

    let shard_list = [shard_hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

    assert_eq!(stats.shards_bloom_skipped, 0);
    assert_eq!(stats.shards_downloaded, 1);
    assert!(!idx.has_shard(&shard_hash));
}

#[tokio::test]
async fn bloom_filter_falls_back_for_v1_shard() {
    let (router, cache, _dir) = setup();

    // Build a canonical v1 shard without a bloom.
    let (shard_bytes, shard_hash) = build_plain_shard();

    let obj_path = router.shard_path(&shard_hash);
    router
        .store()
        .put(&obj_path, Bytes::from(shard_bytes))
        .await
        .unwrap();

    // Even with query hashes set, no-bloom shards download normally.
    let query: HashSet<MerkleHash> = (200..210).map(|i| MerkleHash::from([i, i, i, i])).collect();

    let mut sync = ShardSynchronizer::new(router, cache, None).with_query_hashes(query);
    let mut idx = ChunkIndex::new();

    let shard_list = [shard_hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

    assert_eq!(stats.shards_bloom_skipped, 0);
    assert_eq!(stats.shards_downloaded, 1);
    assert!(!idx.has_shard(&shard_hash));
}

#[tokio::test]
async fn query_scoped_sync_does_not_mark_persistent_shard_complete() {
    let (router, cache, dir) = setup();

    let shard_chunks: Vec<MerkleHash> = (3000..3100)
        .map(|i: u64| {
            MerkleHash::from([
                i,
                i.wrapping_mul(31),
                i.wrapping_mul(97),
                i.wrapping_mul(127),
            ])
        })
        .collect();
    let (shard_bytes, shard_hash) = build_bloom_shard(&shard_chunks);

    let obj_path = router.shard_path(&shard_hash);
    router
        .store()
        .put(&obj_path, Bytes::from(shard_bytes.clone()))
        .await
        .unwrap();

    let persistent_path = dir
        .path()
        .join("repos")
        .join("testhash")
        .join("chunk-index.sqlite");
    let persistent = Arc::new(PersistentChunkIndex::open_or_create(&persistent_path).unwrap());
    let query: HashSet<MerkleHash> = [shard_chunks[0]].into_iter().collect();
    let mut sync = ShardSynchronizer::new(router, cache, None)
        .with_query_hashes(query)
        .with_persistent_index(Arc::clone(&persistent));
    let mut idx = ChunkIndex::new();

    let shard_list = [shard_hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

    assert_eq!(stats.shards_downloaded, 1);
    assert!(!idx.has_shard(&shard_hash));
    assert!(!persistent.has_shard(&shard_hash).unwrap());

    sync.install_shard_data(&shard_hash, &Bytes::from(shard_bytes), &mut idx, false);

    assert!(idx.has_shard(&shard_hash));
    assert!(persistent.has_shard(&shard_hash).unwrap());
}

#[tokio::test]
async fn exact_query_sync_installs_every_requested_match_only() {
    let (router, cache, _dir) = setup();
    let (shard_bytes, shard_hash, requested, expected_ref) = build_shard_with_one_chunk();
    router
        .store()
        .put(&router.shard_path(&shard_hash), Bytes::from(shard_bytes))
        .await
        .unwrap();
    let mut sync = ShardSynchronizer::new(router, cache, None)
        .with_exact_query_hashes([requested].into_iter().collect());
    let mut idx = ChunkIndex::new();

    let stats = sync.sync(&mut idx, &[shard_hash.hex()]).await.unwrap();

    assert_eq!(stats.shards_downloaded, 1);
    assert_eq!(idx.get(&requested), Some(&expected_ref));
    assert_eq!(idx.len(), 1);
    assert!(!idx.has_shard(&shard_hash));
}

#[tokio::test]
async fn full_shard_install_repairs_marker_only_persistent_index() {
    let (router, cache, dir) = setup();
    let (shard_bytes, shard_hash, chunk_hash, xorb_ref) = build_shard_with_one_chunk();
    let persistent_path = dir
        .path()
        .join("repos")
        .join("testhash")
        .join("chunk-index.sqlite");
    let persistent = Arc::new(PersistentChunkIndex::open_or_create(&persistent_path).unwrap());

    persistent.install_shard(shard_hash, &[]).unwrap();
    assert!(persistent.has_shard(&shard_hash).unwrap());
    assert!(persistent.get(&chunk_hash).unwrap().is_none());

    let mut sync =
        ShardSynchronizer::new(router, cache, None).with_persistent_index(Arc::clone(&persistent));
    let mut idx = ChunkIndex::new();

    sync.install_shard_data(&shard_hash, &Bytes::from(shard_bytes), &mut idx, false);

    assert_eq!(persistent.get(&chunk_hash).unwrap(), Some(xorb_ref));
    assert_eq!(idx.get(&chunk_hash).copied(), Some(xorb_ref));
    assert!(idx.has_shard(&shard_hash));
}

#[tokio::test]
async fn bloom_filter_disabled_without_query_hashes() {
    let (router, cache, _dir) = setup();

    // Build a canonical v1 shard with a bloom.
    let shard_chunks: Vec<MerkleHash> = (1000..1100)
        .map(|i: u64| {
            MerkleHash::from([
                i,
                i.wrapping_mul(31),
                i.wrapping_mul(97),
                i.wrapping_mul(127),
            ])
        })
        .collect();
    let (shard_bytes, shard_hash) = build_bloom_shard(&shard_chunks);

    let obj_path = router.shard_path(&shard_hash);
    router
        .store()
        .put(&obj_path, Bytes::from(shard_bytes))
        .await
        .unwrap();

    // No with_query_hashes — bloom filtering should be skipped entirely.
    let mut sync = ShardSynchronizer::new(router, cache, None);
    let mut idx = ChunkIndex::new();

    let shard_list = [shard_hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

    assert_eq!(stats.shards_bloom_skipped, 0);
    assert_eq!(stats.shards_downloaded, 1);
    assert!(idx.has_shard(&shard_hash));
}

#[tokio::test]
async fn shard_spills_to_disk_when_chunk_index_over_ceiling() {
    let (router, cache, dir) = setup();
    let shard_cache_dir = dir.path().join("repos").join("testhash").join("shards");

    // Build a valid plain v1 shard and upload it.
    let (shard_bytes, shard_hash) = build_plain_shard();
    let obj_path = router.shard_path(&shard_hash);
    router
        .store()
        .put(&obj_path, Bytes::from(shard_bytes))
        .await
        .unwrap();

    // Use a ChunkIndex with a very low ceiling and pre-populate it
    // with entries so it's already over the ceiling before sync.
    let mut idx = ChunkIndex::with_ceiling(40);
    let dummy_hash = MerkleHash::from([1u64, 1, 1, 1]);
    let dummy_ref = crab_xet::xorb::format::XorbRef {
        xorb_hash: MerkleHash::from([2u64, 2, 2, 2]),
        chunk_index: 0,
        uncompressed_size: 100,
    };
    idx.insert(dummy_hash, dummy_ref);
    idx.insert(MerkleHash::from([3u64, 3, 3, 3]), dummy_ref);
    assert!(idx.over_ceiling());

    let mut sync =
        ShardSynchronizer::new(router, cache, None).with_shard_cache_dir(shard_cache_dir.clone());

    let shard_list = [shard_hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

    assert_eq!(stats.shards_downloaded, 1);
    // The shard is marked as installed (so it won't be re-downloaded).
    assert!(idx.has_shard(&shard_hash));
    // An MDBShardFile handle was created for on-disk querying.
    assert_eq!(sync.shard_files().len(), 1);
    // The shard cache directory was created.
    assert!(shard_cache_dir.exists());
}

#[tokio::test]
async fn shard_loads_into_memory_when_under_ceiling() {
    let (router, cache, dir) = setup();
    let shard_cache_dir = dir.path().join("repos").join("testhash").join("shards");

    // Build a valid plain v1 shard and upload it.
    let (shard_bytes, shard_hash) = build_plain_shard();
    let obj_path = router.shard_path(&shard_hash);
    router
        .store()
        .put(&obj_path, Bytes::from(shard_bytes))
        .await
        .unwrap();

    // Default ceiling (1 GiB) — should load into memory, not disk.
    let mut idx = ChunkIndex::new();

    let mut sync =
        ShardSynchronizer::new(router, cache, None).with_shard_cache_dir(shard_cache_dir);

    let shard_list = [shard_hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

    assert_eq!(stats.shards_downloaded, 1);
    assert!(idx.has_shard(&shard_hash));
    // No on-disk fallback handles created.
    assert_eq!(sync.shard_files().len(), 0);
}

#[tokio::test]
async fn shard_falls_back_to_memory_without_shard_cache_dir() {
    let (router, cache, _dir) = setup();

    // Build a valid plain v1 shard and upload it.
    let (shard_bytes, shard_hash) = build_plain_shard();
    let obj_path = router.shard_path(&shard_hash);
    router
        .store()
        .put(&obj_path, Bytes::from(shard_bytes))
        .await
        .unwrap();

    // Ceiling of 0 but no shard_cache_dir — should fall back to
    // in-memory install since there's nowhere to spill.
    let mut idx = ChunkIndex::with_ceiling(0);

    let mut sync = ShardSynchronizer::new(router, cache, None);

    let shard_list = [shard_hash.hex()];
    let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

    assert_eq!(stats.shards_downloaded, 1);
    assert!(idx.has_shard(&shard_hash));
    assert_eq!(sync.shard_files().len(), 0);
}
