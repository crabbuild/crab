use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crab_xet::hash::MerkleHash;
use crab_xet::shard::{
    FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo,
    XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};
use object_store::memory::InMemory;

use super::*;
use crab_cache_store::CacheConfig;
use crab_cache_store::CachingStore;
use crab_storage::Store;
use crab_xet::shard::ShardWriter;

fn hash_from_seed(seed: u64) -> MerkleHash {
    MerkleHash::from([
        seed,
        seed.wrapping_mul(31),
        seed.wrapping_mul(97),
        seed.wrapping_mul(127),
    ])
}

fn four_chunk_xorb(xorb_hash: MerkleHash, seed: u64) -> Arc<MDBXorbInfo> {
    let chunks = (0_u32..4)
        .map(|index| {
            XorbChunkSequenceEntry::new(
                hash_from_seed(seed.wrapping_add(u64::from(index))),
                1024,
                index * 1024,
            )
        })
        .collect();
    Arc::new(MDBXorbInfo {
        metadata: XorbChunkSequenceHeader::new(xorb_hash, 4, 4096),
        chunks,
    })
}

fn test_client() -> (StoreClient, tempfile::TempDir) {
    let inner = Arc::new(InMemory::new());
    let origin = Store::new(inner);
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let local_cache = Arc::new(crab_cache::LocalCache::new(cache_dir.path().join("cache")));
    let caching = CachingStore::new_with_local_cache(origin, CacheConfig::default(), local_cache)
        .expect("CachingStore builds with default config");
    let router = StoreLayout::new(caching.origin().clone(), "org/test".to_string());

    let concurrency = AdaptiveConcurrencyController::new_fixed(
        xet_runtime::core::XetContext::default().expect("xet context"),
        "crab-hydrate-test",
        2,
    );
    let client = StoreClient::new(caching, router, concurrency);
    (client, cache_dir)
}

#[test]
fn store_client_uses_caching_store_local_cache() {
    let origin = Store::new(Arc::new(InMemory::new()));
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let local_cache = Arc::new(crab_cache::LocalCache::new(cache_dir.path().join("cache")));
    let caching = CachingStore::new_with_local_cache(origin, CacheConfig::default(), local_cache)
        .expect("caching store");
    let expected = Arc::clone(caching.local_cache());
    let router = StoreLayout::new(caching.origin().clone(), "org/cache-owner".to_owned());
    let concurrency = AdaptiveConcurrencyController::new_download(
        xet_runtime::core::XetContext::default().expect("xet context"),
        "crab-cache-owner-test",
    );

    let client = StoreClient::new(caching, router, concurrency);

    assert!(Arc::ptr_eq(&expected, client.store.local_cache()));
}

async fn seed_file_index(client: &StoreClient, entries: &[(MerkleHash, MerkleHash)]) {
    crab_metadata::layout_descriptor::ensure_canonical_layout(
        client.store.origin(),
        &client.router,
    )
    .await
    .expect("initialize canonical layout");
    let shard_hashes = entries
        .iter()
        .map(|(_, shard_hash)| shard_hash.hex())
        .collect::<Vec<_>>();
    let (shard_index_hash, _, shard_write) = crab_metadata::manifests::append_shard_index(
        crab_metadata::segmented::SegmentIndex::default(),
        1,
        &shard_hashes,
    )
    .expect("build shard index");
    crab_metadata::manifest_store::upload_segmented_bulk(
        client.store.origin(),
        &client.router,
        &crab_metadata::manifests::BulkData {
            shard_index: shard_write,
            pack_index: crab_metadata::segmented::SegmentWrite::default(),
        },
    )
    .await
    .expect("upload shard index");
    let mut manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    manifest.shard_index_hash = shard_index_hash.clone();
    manifest.seal_git_validation();
    crab_metadata::manifest_store::create_manifest(
        client.store.origin(),
        &client.router,
        &manifest,
    )
    .await
    .expect("create manifest");

    let shard_index_hash = MerkleHash::from_hex(&shard_index_hash).expect("index hash");
    let db = slatedb::Db::open(
        object_store::path::Path::from(format!("{}/file_index_db/", client.router.repo_prefix())),
        Arc::clone(client.store.origin().inner()),
    )
    .await
    .expect("open file-index writer");
    let mut result = Ok(());
    for (file_hash, shard_hash) in entries {
        result = db
            .put(
                crab_metadata::key_codec::encode_committed_file_key(file_hash, 1),
                crab_metadata::value_codec::encode_committed_file_record(
                    &crab_metadata::value_codec::CommittedFileRecord {
                        recipe_hash: [0xC8; 32],
                        shard_hash: *shard_hash,
                        committed_generation: 1,
                        shard_index_hash,
                    },
                ),
            )
            .await
            .map(|_| ());
        if result.is_err() {
            break;
        }
    }
    let closed = db.close().await;
    result.expect("write committed records");
    closed.expect("close file-index writer");
}

#[test]
fn xorb_url_round_trip_single_range() {
    let hash = hash_from_seed(42);
    let range = ChunkRange::new(3, 10);
    let url = xorb_url(&hash, &[range]);
    let (parsed_hash, parsed_ranges) = parse_xorb_url(&url).expect("parse");
    assert_eq!(parsed_hash, hash);
    assert_eq!(parsed_ranges, vec![range]);
}

#[test]
fn xorb_url_round_trip_multi_range() {
    let hash = hash_from_seed(7);
    let ranges = vec![
        ChunkRange::new(0, 5),
        ChunkRange::new(8, 12),
        ChunkRange::new(20, 21),
    ];
    let url = xorb_url(&hash, &ranges);
    let (parsed_hash, parsed_ranges) = parse_xorb_url(&url).expect("parse");
    assert_eq!(parsed_hash, hash);
    assert_eq!(parsed_ranges, ranges);
}

#[test]
fn xorb_url_rejects_foreign_prefix() {
    let err = parse_xorb_url("https://example.com/xorb").expect_err("should reject non-crab url");
    match err {
        ClientError::Other(msg) => assert!(msg.contains("unrecognized")),
        other => panic!("expected Other, got {other:?}"),
    }
}

#[test]
fn xorb_url_rejects_bad_hash() {
    let err =
        parse_xorb_url("crab-xorb://not-hex?chunks=0-1").expect_err("should reject non-hex hash");
    match err {
        ClientError::Other(msg) => assert!(msg.contains("invalid xorb url hash")),
        other => panic!("expected Other, got {other:?}"),
    }
}

#[test]
fn build_response_v2_covers_every_segment() {
    let file_hash = hash_from_seed(1);
    let xorb_a = hash_from_seed(2);
    let xorb_b = hash_from_seed(3);
    let info = MDBFileInfo {
        metadata: FileDataSequenceHeader::new(file_hash, 2u32, false, false),
        segments: vec![
            FileDataSequenceEntry::new(xorb_a, 1024u32, 0u32, 4u32),
            FileDataSequenceEntry::new(xorb_b, 2048u32, 5u32, 13u32),
        ],
        verification: vec![],
        metadata_ext: None,
    };

    let response = build_response_v2(&info);
    assert_eq!(response.terms.len(), 2);
    assert_eq!(response.terms[0].range, ChunkRange::new(0, 4));
    assert_eq!(response.terms[0].unpacked_length, 1024);
    assert_eq!(response.terms[1].range, ChunkRange::new(5, 13));
    assert_eq!(response.xorbs.len(), 2);

    let fetches_a = &response.xorbs[&HexMerkleHash::from(xorb_a)];
    assert_eq!(fetches_a.len(), 1);
    let (parsed_hash, parsed_ranges) =
        parse_xorb_url(&fetches_a[0].url).expect("url encodes xorb ref");
    assert_eq!(parsed_hash, xorb_a);
    assert_eq!(parsed_ranges, vec![ChunkRange::new(0, 4)]);
}

#[test]
fn build_response_v2_represents_zero_byte_file() {
    let file_hash = hash_from_seed(4);
    let info = MDBFileInfo {
        metadata: FileDataSequenceHeader::new(file_hash, 0u32, false, false),
        segments: vec![],
        verification: vec![],
        metadata_ext: None,
    };

    let response = build_response_v2(&info);
    assert!(response.terms.is_empty());
    assert!(response.xorbs.is_empty());
    assert_eq!(response.offset_into_first_range, 0);
}

#[test]
fn reconstruction_ranges_preserve_segment_offsets_and_eof() {
    let xorb_a = hash_from_seed(2);
    let xorb_b = hash_from_seed(3);
    let info = MDBFileInfo {
        metadata: FileDataSequenceHeader::new(hash_from_seed(1), 2, false, false),
        segments: vec![
            FileDataSequenceEntry::new(xorb_a, 1024, 0, 4),
            FileDataSequenceEntry::new(xorb_b, 2048, 5, 13),
        ],
        verification: vec![],
        metadata_ext: None,
    };
    let xorb_info = |hash: MerkleHash, count: u32| {
        Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(hash, count, count * 256),
            chunks: (0..count)
                .map(|index| {
                    XorbChunkSequenceEntry::new(
                        hash_from_seed(100 + u64::from(index)),
                        256,
                        index * 256,
                    )
                })
                .collect(),
        })
    };
    let mut writer = ShardWriter::new();
    writer.add_xorb(xorb_info(xorb_a, 4)).unwrap();
    writer.add_xorb(xorb_info(xorb_b, 13)).unwrap();
    writer.add_file(info.clone()).unwrap();
    let (bytes, hash) = writer.finalize().unwrap();
    let shard = ShardReader::from_bytes(Bytes::from(bytes), hash);
    for (start, end, expected) in [
        (
            512,
            2048,
            Some((0, vec![ChunkRange::new(2, 4), ChunkRange::new(5, 9)])),
        ),
        (1024, 3072, Some((0, vec![ChunkRange::new(5, 13)]))),
        (1536, 4096, Some((0, vec![ChunkRange::new(7, 13)]))),
        (600, 700, Some((88, vec![ChunkRange::new(2, 3)]))),
        (3072, 4096, None),
    ] {
        let actual = build_range_response_v2(&info, &shard, FileRange::new(start, end))
            .unwrap()
            .map(|response| {
                (
                    response.offset_into_first_range,
                    response
                        .terms
                        .into_iter()
                        .map(|term| term.range)
                        .collect::<Vec<_>>(),
                )
            });
        assert_eq!(actual, expected, "byte range {start}..{end}");
    }
    assert!(matches!(
        build_range_response_v2(&info, &shard, FileRange::new(2, 1)),
        Err(ClientError::InvalidRange)
    ));
}

#[tokio::test]
async fn get_file_reconstruction_info_returns_none_for_unknown_file() {
    let (client, _tmp) = test_client();
    let unknown = hash_from_seed(999);
    let result = client
        .get_file_reconstruction_info(&unknown)
        .await
        .expect("not-found path returns Ok(None)");
    assert!(result.is_none());
}

#[tokio::test]
async fn get_reconstruction_errors_for_unknown_file() {
    let (client, _tmp) = test_client();
    let unknown = hash_from_seed(1234);
    let err = client.get_reconstruction(&unknown, None).await.expect_err(
        "shard-missing must error out — returning Ok(None) would \
             cause a silent 0-byte reconstruction",
    );
    match err {
        ClientError::Other(msg) => {
            assert!(
                msg.contains("shard not found"),
                "expected shard error, got {msg}"
            );
        }
        other => panic!("expected Other, got {other:?}"),
    }
}

#[tokio::test]
async fn shard_hint_resolves_without_file_index_db() {
    let (client, _tmp) = test_client();
    let file_hash = hash_from_seed(52);
    let xorb_hash = hash_from_seed(53);
    let file_info = MDBFileInfo {
        metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
        segments: vec![FileDataSequenceEntry::new(xorb_hash, 4096u32, 0u32, 4u32)],
        verification: vec![],
        metadata_ext: None,
    };

    let mut shard = ShardWriter::new();
    shard
        .add_xorb(four_chunk_xorb(xorb_hash, 5_300))
        .expect("add xorb");
    shard.add_file(file_info).expect("add file");
    let (shard_bytes, shard_hash) = shard.finalize().expect("finalize shard");
    client
        .store
        .origin()
        .put(
            &client.router.shard_path(&shard_hash),
            Bytes::from(shard_bytes),
        )
        .await
        .expect("upload shard");

    let hinted = client.with_shard_hint(file_hash, shard_hash);
    let (info, resolved_shard) = hinted
        .get_file_reconstruction_info(&file_hash)
        .await
        .expect("hint lookup succeeds")
        .expect("hinted shard contains file");

    assert_eq!(resolved_shard, Some(shard_hash));
    assert_eq!(info.segments.len(), 1);
    assert_eq!(info.segments[0].xorb_hash, xorb_hash);
}

#[tokio::test]
async fn shard_hint_hit_updates_metrics() {
    let (client, _tmp) = test_client();
    let metrics = Arc::new(Metrics::default());
    let file_hash = hash_from_seed(62);
    let xorb_hash = hash_from_seed(63);
    let file_info = MDBFileInfo {
        metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
        segments: vec![FileDataSequenceEntry::new(xorb_hash, 4096u32, 0u32, 4u32)],
        verification: vec![],
        metadata_ext: None,
    };

    let mut shard = ShardWriter::new();
    shard
        .add_xorb(four_chunk_xorb(xorb_hash, 6_300))
        .expect("add xorb");
    shard.add_file(file_info).expect("add file");
    let (shard_bytes, shard_hash) = shard.finalize().expect("finalize shard");
    client
        .store
        .origin()
        .put(
            &client.router.shard_path(&shard_hash),
            Bytes::from(shard_bytes),
        )
        .await
        .expect("upload shard");

    let hinted = client
        .with_metrics(Arc::clone(&metrics))
        .with_shard_hint(file_hash, shard_hash);
    let _ = hinted
        .get_file_reconstruction_info(&file_hash)
        .await
        .expect("hint lookup succeeds")
        .expect("hinted shard contains file");

    assert_eq!(metrics.counts(), (1, 0));
}

#[tokio::test]
async fn stale_shard_hint_updates_miss_metrics() {
    let (client, _tmp) = test_client();
    let metrics = Arc::new(Metrics::default());
    let file_hash = hash_from_seed(72);
    let missing_shard_hash = hash_from_seed(73);

    let hinted = client
        .with_metrics(Arc::clone(&metrics))
        .with_shard_hint(file_hash, missing_shard_hash);
    let result = hinted
        .get_file_reconstruction_info(&file_hash)
        .await
        .expect("stale hint falls through to canonical not found");
    assert!(result.is_none());

    assert_eq!(metrics.counts(), (0, 1));
}

#[tokio::test]
async fn acquire_download_permit_succeeds() {
    let (client, _tmp) = test_client();
    let _permit = client
        .acquire_download_permit()
        .await
        .expect("download permit available");
}

#[tokio::test]
async fn upload_paths_are_rejected() {
    let (client, _tmp) = test_client();

    let err = client
        .acquire_upload_permit()
        .await
        .err()
        .expect("acquire_upload_permit must report unsupported");
    assert!(matches!(err, ClientError::Other(_)));

    let permit = client.acquire_download_permit().await.unwrap();
    let err = client
        .upload_shard(Bytes::new(), permit, None)
        .await
        .expect_err("upload_shard must report unsupported");
    assert!(matches!(err, ClientError::Other(_)));
}

#[tokio::test]
async fn batch_get_reconstruction_empty_is_empty() {
    let (client, _tmp) = test_client();
    let response = client
        .batch_get_reconstruction(&[])
        .await
        .expect("empty batch succeeds");
    assert!(response.files.is_empty());
    assert!(response.fetch_info.is_empty());
}

#[tokio::test]
async fn abandoned_term_batches_remove_managed_reader_checkpoints() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    use std::time::Duration;
    use tokio::time::timeout;

    for (mode, during_cleanup) in [
        ("terms", false),
        ("terms", true),
        ("sequences", false),
        ("sequences", true),
        ("strict", false),
        ("strict", true),
    ] {
        let (client, _tmp) = test_client();
        let file_hash = hash_from_seed(42);
        seed_file_index(&client, &[(file_hash, hash_from_seed(43))]).await;
        let inner = Arc::clone(client.store.origin().inner());
        let admin = slatedb::admin::Admin::builder(
            format!("{}/file_index_db/", client.router.repo_prefix()),
            Arc::clone(&inner),
        )
        .build();
        assert!(admin.list_checkpoints(None).await.unwrap().is_empty());
        // Keep origin reads pending long enough to observe reader ownership,
        // without changing the caller's cancellation token or the stored index.
        let throttled = Arc::new(ThrottledStore::new(
            inner,
            ThrottleConfig {
                wait_get_per_call: Duration::from_millis(20),
                ..ThrottleConfig::default()
            },
        ));
        let origin = Store::new(throttled.clone());
        let cache = Arc::clone(client.store.local_cache());
        let store = CachingStore::new_with_local_cache(
            origin.clone(),
            CacheConfig::default(),
            Arc::clone(&cache),
        )
        .unwrap();
        let layout = StoreLayout::new(origin, client.router.repo_prefix().to_owned());
        let resolver = crate::TermResolver::new(store, layout, cache, 1).unwrap();
        let cache_owners = Arc::strong_count(client.store.local_cache());
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut batch = Box::pin(async {
            let terms = [(file_hash, None)];
            let files = [(file_hash, None, 16)];
            let source = crab_diff::types::ChunkSequenceSourceKind::Committed;
            match mode {
                "terms" => resolver.resolve_batch(&terms, &cancel).await.map(|_| ()),
                "sequences" => resolver
                    .resolve_sequences_batch(&files, source, &cancel)
                    .await
                    .map(|_| ()),
                _ => resolver
                    .resolve_sequences_batch_strict(&files, source, &cancel)
                    .await
                    .map(|_| ()),
            }
        });
        timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut batch => panic!("batch ended before checkpoint observation: {result:?}"),
                    checkpoints = admin.list_checkpoints(None) => {
                        let workers_released = Arc::strong_count(client.store.local_cache()) == cache_owners;
                        if !checkpoints.unwrap().is_empty() && (!during_cleanup || workers_released) {
                            break;
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reader reached the requested abandonment phase");
        drop(batch);
        throttled.config_mut(|config| config.wait_get_per_call = Duration::ZERO);
        timeout(Duration::from_secs(2), async {
            while !admin.list_checkpoints(None).await.unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{mode} abandoned its managed reader checkpoint (during_cleanup={during_cleanup})"
            )
        });
    }
}

#[tokio::test]
async fn file_index_resolution_agrees_between_term_and_reconstruction_batches() {
    let (client, _tmp) = test_client();
    let file_hash = hash_from_seed(42);
    let missing_hash = hash_from_seed(43);
    let xorb_hash = hash_from_seed(44);
    let file_info = MDBFileInfo {
        metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
        segments: vec![FileDataSequenceEntry::new(xorb_hash, 2048u32, 2u32, 6u32)],
        verification: vec![],
        metadata_ext: None,
    };

    let mut shard = ShardWriter::new();
    let xorb_chunks = (0..6u64)
        .map(|index| {
            XorbChunkSequenceEntry::new(
                hash_from_seed(100 + index),
                512,
                u32::try_from(index * 512).expect("test offset fits u32"),
            )
        })
        .collect::<Vec<_>>();
    shard
        .add_xorb(Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb_hash, 6, 3072),
            chunks: xorb_chunks,
        }))
        .expect("add xorb metadata");
    shard.add_file(file_info).expect("add file");
    let (shard_bytes, shard_hash) = shard.finalize().expect("finalize shard");
    client
        .store
        .origin()
        .put(
            &client.router.shard_path(&shard_hash),
            Bytes::from(shard_bytes),
        )
        .await
        .expect("upload shard");
    seed_file_index(&client, &[(file_hash, shard_hash)]).await;

    let resolver = crate::TermResolver::new(
        client.store.clone(),
        client.router.clone(),
        Arc::clone(client.store.local_cache()),
        2,
    )
    .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let resolved = resolver
        .resolve_batch(&[(file_hash, None), (missing_hash, None)], &cancel)
        .await
        .unwrap();
    assert_eq!(
        resolved,
        [(
            file_hash,
            vec![FileDataSequenceEntry::new(xorb_hash, 2048, 2, 6)]
        )]
        .into_iter()
        .collect()
    );
    for strict in [false, true] {
        let files = [(file_hash, None, 2048)];
        let source = crab_diff::types::ChunkSequenceSourceKind::Committed;
        let sequences = if strict {
            resolver
                .resolve_sequences_batch_strict(&files, source, &cancel)
                .await
        } else {
            resolver
                .resolve_sequences_batch(&files, source, &cancel)
                .await
        }
        .unwrap();
        let chunks = sequences[&file_hash]
            .spans
            .iter()
            .map(|span| (span.chunk_hash, span.offset, span.len))
            .collect::<Vec<_>>();
        assert_eq!(
            chunks,
            (0..4)
                .map(|index| (hash_from_seed(102 + index), index * 512, 512))
                .collect::<Vec<_>>()
        );
    }

    let response = client
        .batch_get_reconstruction(&[file_hash, missing_hash])
        .await
        .expect("batch reconstruction");
    let file_key = HexMerkleHash::from(file_hash);
    let missing_key = HexMerkleHash::from(missing_hash);

    assert!(!response.files.contains_key(&missing_key));
    let terms = response.files.get(&file_key).expect("hit terms");
    assert_eq!(terms.len(), 1);
    assert_eq!(terms[0].hash, HexMerkleHash::from(xorb_hash));
    assert_eq!(terms[0].unpacked_length, 2048);
    assert_eq!(terms[0].range, ChunkRange::new(2, 6));

    let fetches = response
        .fetch_info
        .get(&HexMerkleHash::from(xorb_hash))
        .expect("xorb fetch info");
    assert_eq!(fetches.len(), 1);
    assert_eq!(fetches[0].range, ChunkRange::new(2, 6));
    let (parsed_hash, parsed_ranges) = parse_xorb_url(&fetches[0].url).expect("fetch url");
    assert_eq!(parsed_hash, xorb_hash);
    assert_eq!(parsed_ranges, vec![ChunkRange::new(2, 6)]);
}

// Xet's ChunkCache::put requires N+1 offsets, from zero through data.len().
// Preserve this contract so hydration can populate the decoded-range cache.
#[tokio::test]
async fn get_file_term_data_offsets_match_xet_core_contract() {
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;

    // Build a real xorb with 5 chunks so the offset Vec is non-trivial.
    let mut builder = XorbBuilder::new();
    for i in 0u32..5 {
        let size = 1024 + (i as usize) * 128;
        let data: Vec<u8> = (0..size as u32)
            .map(|j| (j.wrapping_mul(i.wrapping_mul(2654435761))) as u8)
            .collect();
        let chunk = Chunk::new(Bytes::from(data));
        builder.push(&chunk, RunId(0)).unwrap();
    }
    let xorbs = builder.finalize().unwrap();
    let xorb = xorbs.into_iter().next().expect("one xorb");

    let (client, _tmp) = test_client();
    let xorb_path = client.router.xorb_path(&xorb.hash);
    client
        .store
        .origin()
        .put(&xorb_path, xorb.bytes.clone())
        .await
        .expect("upload xorb");

    // Single-range URL spanning every chunk in the xorb.
    let range = ChunkRange::new(0, 5);
    let url = xorb_url(&xorb.hash, std::slice::from_ref(&range));

    let permit = client
        .acquire_download_permit()
        .await
        .expect("download permit");
    let (data, offsets) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.get_file_term_data(Box::new(FixedURL { url }), permit, None, None),
    )
    .await
    .expect("cache-owned term fetch must not deadlock")
    .expect("fetch term data");

    // xet-core contract: one offset per chunk plus a trailing length.
    assert_eq!(offsets.len(), 6, "5 chunks + 1 trailing offset");
    assert_eq!(offsets[0], 0, "first offset must be 0");
    assert_eq!(
        offsets[5] as usize,
        data.len(),
        "last offset must equal data length",
    );

    // Offsets must be strictly increasing (DiskCache::put validator).
    for pair in offsets.windows(2) {
        assert!(pair[0] < pair[1], "offsets must be strictly increasing");
    }

    let cache_root = client.store.local_cache().root().join("chunks");
    let cache = crab_cache::XetChunkCacheHandle::open(&cache_root, 1024 * 1024)
        .expect("decoded-range cache")
        .cache;
    let key = xet_client::cas_types::Key {
        prefix: String::new(),
        hash: xorb.hash,
    };
    cache
        .put(&key, &range, &offsets, &data)
        .await
        .expect("accept adapter offsets");
    let hit = cache
        .get(&key, &range)
        .await
        .expect("read decoded range")
        .expect("cache hit");
    assert_eq!((hit.data.as_slice(), hit.offsets), (data.as_ref(), offsets));
}

#[tokio::test]
async fn get_file_term_data_recovers_corruption_without_installing_full_xorb() {
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;

    let chunks: Vec<Bytes> = vec![
        Bytes::from_static(b"first chunk payload"),
        Bytes::from_static(b"second chunk payload"),
    ];
    let mut builder = XorbBuilder::new();
    for chunk in &chunks {
        builder.push(&Chunk::new(chunk.clone()), RunId(0)).unwrap();
    }
    let xorb = builder.finalize().unwrap().pop().expect("one xorb");

    let (client, _tmp) = test_client();
    let store_cache = Arc::clone(client.store.local_cache());
    let xorb_path = client.router.xorb_path(&xorb.hash);
    client
        .store
        .origin()
        .put(&xorb_path, xorb.bytes.clone())
        .await
        .expect("upload xorb");
    store_cache
        .put_unchecked_for_test(&CacheKey::Xorb(xorb.hash), b"not a valid xorb")
        .await
        .expect("seed corrupt store xorb");

    let range = ChunkRange::new(0, chunks.len() as u32);
    let url = xorb_url(&xorb.hash, std::slice::from_ref(&range));

    let permit = client
        .acquire_download_permit()
        .await
        .expect("download permit");
    let (data, offsets) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.get_file_term_data(Box::new(FixedURL { url }), permit, None, None),
    )
    .await
    .expect("cache repair must not deadlock")
    .expect("corrupt local xorb should be repaired from origin");

    let expected = chunks
        .iter()
        .flat_map(|chunk| chunk.iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(data.as_ref(), expected.as_slice());
    assert_eq!(
        offsets,
        vec![0, chunks[0].len() as u32, expected.len() as u32]
    );

    // Ordinary hydration repairs the read, not the full-xorb cache.
    assert!(!store_cache.contains(&CacheKey::Xorb(xorb.hash)).await);
}

#[derive(Default)]
struct Metrics {
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Metrics {
    fn counts(&self) -> (u64, u64) {
        (self.hits.load(Relaxed), self.misses.load(Relaxed))
    }
}

impl ReadMetrics for Metrics {
    fn shard_hint_hit(&self) {
        self.hits.fetch_add(1, Relaxed);
    }
    fn shard_hint_miss(&self) {
        self.misses.fetch_add(1, Relaxed);
    }
}

struct FixedURL {
    url: String,
}

#[async_trait::async_trait]
impl URLProvider for FixedURL {
    async fn retrieve_url(&self) -> ClientResult<(String, Vec<HttpRange>)> {
        Ok((self.url.clone(), vec![]))
    }
    async fn refresh_url(&self) -> ClientResult<()> {
        Ok(())
    }
}
