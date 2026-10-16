use super::*;
use crab_metadata::git_object_locator::{PagedGitObjectIndex, PagedGitObjectIndexWrite};
use crab_metadata::manifests::{Manifest, PackManifestEntry, compact_pack_index};
use crab_storage::StoreLayout;
use object_store::{ObjectStoreExt, memory::InMemory};
use std::io::Write as _;

async fn fixture() -> (Arc<RemoteGitReader>, gix_hash::ObjectId) {
    let encode = |header: Header, data: &[u8]| {
        let mut bytes = Vec::new();
        header.write_to(data.len() as u64, &mut bytes).unwrap();
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        bytes.extend(encoder.finish().unwrap());
        bytes
    };
    let oid = |data: &[u8]| {
        gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, data).unwrap()
    };
    let base = encode(Header::Blob, b"hello world");
    let delta = encode(
        Header::OfsDelta {
            base_distance: base.len() as u64,
        },
        &[0x0b, 0x0c, 0x90, 0x0b, 0x01, b'!'],
    );
    let mut pack = b"PACK\0\0\0\x02\0\0\0\x02".to_vec();
    pack.extend_from_slice(&base);
    pack.extend_from_slice(&delta);
    let checksum = sha1::Sha1::digest(&pack);
    pack.extend_from_slice(&checksum);
    let pack_id = MerkleHash::from_hex(blake3::hash(&pack).to_hex().as_str()).unwrap();
    let inventory = GitPackInventoryEntry {
        pack_id,
        object_count: 2,
        pack_size: pack.len() as u64,
    };
    let target = oid(b"hello world!");
    let entries = [
        (oid(b"hello world"), &base, 12),
        (target, &delta, 12 + base.len() as u64),
    ]
    .map(|(oid, bytes, offset)| {
        (
            oid.as_bytes().try_into().unwrap(),
            GitObjectLocator {
                ordinal: 0,
                pack_id,
                location: GitObjectLocation {
                    pack_offset: offset,
                    entry_len: bytes.len() as u64,
                    crc32: gix_features::hash::crc32(bytes),
                },
                metadata: Default::default(),
            },
        )
    });
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "repository".to_owned());
    store
        .put(&repo_pack_path("repository", &pack_id), Bytes::from(pack))
        .await
        .unwrap();
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    manifest.git_validation_digest = "b".repeat(64);
    manifest.pack_index_hash = compact_pack_index(
        1,
        &[PackManifestEntry {
            pack_id: pack_id.to_string(),
            size: inventory.pack_size,
            content_hash: pack_id.to_string(),
            ref_tips: Vec::new(),
            object_count: 2,
        }],
    )
    .unwrap()
    .0;
    let write = PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &[inventory], &entries)
        .unwrap();
    let hash = write.upload(&store, &layout).await.unwrap();
    let index = PagedGitObjectIndex::load(
        &store,
        &layout,
        &hash,
        &"c".repeat(64),
        &manifest,
        128 * 1024,
    )
    .await
    .unwrap();
    index
        .verify_complete(&store, &manifest, 128 * 1024)
        .await
        .unwrap();
    let runtime = Arc::new(RemoteGitRuntime::default());
    let reader = RemoteGitReader::from_pinned_with_preferred_pack_indexes(
        store,
        "repository",
        [inventory],
        SnapshotLookupSources::default().with_object_index(index),
        ReaderLimits::default(),
        runtime,
        RepositoryIdentity::new("provider", "repository", 1).unwrap(),
        1,
    )
    .unwrap();
    (Arc::new(reader), target)
}

#[tokio::test]
async fn paged_lookup_reconstructs_offset_deltas_without_pack_sidecars() {
    let (reader, target) = fixture().await;
    let budget = OperationBudget::new(crate::OperationLimits::default(), reader.runtime.clone());
    // Only the pack and paged index exist: any hidden IDX/REV lookup fails.
    let result = reader
        .read_with_session(
            &GitObjectLocatorSession::without_catalog(),
            target,
            &budget,
            &CancellationToken::new(),
        )
        .await;
    reader.runtime.shutdown().await;
    assert_eq!(result.unwrap().data.as_ref(), b"hello world!");
}

#[tokio::test]
async fn paged_lookup_misses_are_definitive_without_pack_sidecars() {
    let (reader, _) = fixture().await;
    let budget = OperationBudget::new(crate::OperationLimits::default(), reader.runtime.clone());
    let result = reader
        .lookup_batch_for_read(
            &GitObjectLocatorSession::without_catalog(),
            &[[9; 20], [9; 20]],
            &budget,
            &CancellationToken::new(),
        )
        .await;
    reader.runtime.shutdown().await;
    assert_eq!(
        result.unwrap(),
        vec![GitObjectLookup::Miss, GitObjectLookup::Miss]
    );
}

#[tokio::test]
async fn corrupt_physical_pages_are_errors_without_pack_index_fallback() {
    let (reader, target) = fixture().await;
    let path = reader.object_index.as_ref().unwrap().body_path();
    let mut bytes = reader.store.get_with_etag(path).await.unwrap().0.to_vec();
    bytes[14] ^= 1;
    // Bypass immutable Store writes to inject corruption at the provider boundary.
    reader
        .store
        .inner()
        .put(path, Bytes::from(bytes).into())
        .await
        .unwrap();
    let budget = OperationBudget::new(crate::OperationLimits::default(), reader.runtime.clone());
    let result = reader
        .lookup_batch_for_read(
            &GitObjectLocatorSession::without_catalog(),
            &[target.as_bytes().try_into().unwrap()],
            &budget,
            &CancellationToken::new(),
        )
        .await;
    reader.runtime.shutdown().await;
    assert!(matches!(result, Err(Error::Metadata(_))));
}

#[tokio::test]
async fn cancelled_physical_lookup_does_not_touch_origin() {
    let (reader, target) = fixture().await;
    let budget = OperationBudget::new(crate::OperationLimits::default(), reader.runtime.clone());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = reader
        .lookup_batch_for_read(
            &GitObjectLocatorSession::without_catalog(),
            &[target.as_bytes().try_into().unwrap()],
            &budget,
            &cancel,
        )
        .await;
    let requests = budget
        .usage()
        .await
        .amount(BudgetDimension::StorageRequests);
    reader.runtime.shutdown().await;
    assert!(matches!(result, Err(Error::Cancelled)) && requests == 0);
}

#[tokio::test]
async fn physical_pages_obey_the_operations_storage_budget() {
    let (reader, target) = fixture().await;
    let budget = OperationBudget::new(
        crate::OperationLimits {
            max_storage_requests: 1,
            ..Default::default()
        },
        reader.runtime.clone(),
    );
    budget
        .charge(BudgetDimension::StorageRequests, 1)
        .await
        .unwrap();
    let result = reader
        .lookup_batch_for_read(
            &GitObjectLocatorSession::without_catalog(),
            &[target.as_bytes().try_into().unwrap()],
            &budget,
            &CancellationToken::new(),
        )
        .await;
    reader.runtime.shutdown().await;
    let Err(Error::Metadata(crab_metadata::error::MetadataError::Storage { source })) = result
    else {
        panic!("expected a source-preserving storage admission failure");
    };
    assert!(crab_storage::read_rejection(&source).is_some());
}
