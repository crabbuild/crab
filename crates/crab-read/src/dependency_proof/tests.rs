use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use bytes::Bytes;
use crab_git::lfs_pointer::LfsPointer;
use crab_metadata::{
    manifest_store,
    manifests::{BulkData, Manifest, compact_shard_index},
};
use crab_types::pointer::Pointer;
use crab_xet::shard::{FileDataSequenceHeader, MDBFileInfo, ShardWriter};
use futures_util::TryStreamExt;
use object_store::{
    ObjectStore,
    memory::InMemory,
    throttle::{ThrottleConfig, ThrottledStore},
};

use super::*;

const LFS_HASH: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

fn lfs(size: u64) -> PointerDependency {
    let pointer = LfsPointer::parse(
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{LFS_HASH}\nsize {size}\n")
            .as_bytes(),
    )
    .unwrap();
    PointerDependency {
        blob: ObjectId::from_bytes_or_panic(&[2; 20]),
        pointer: PointerKind::Lfs(pointer),
    }
}

fn limits() -> DependencyProofLimits {
    DependencyProofLimits {
        max_dependencies: 8,
        max_total_file_bytes: 1024,
        max_duration: Duration::from_secs(10),
        lookup: FileIndexLookupLimits {
            max_files: 8,
            max_shard_visits: 8,
            max_shard_bytes: 65536,
            max_recipe_entries: 16,
        },
        content: PointerProofLimits {
            max_file_bytes: 1024,
            max_shard_bytes: 65536,
            max_xorb_bytes: 65536,
            max_read_bytes: 65536,
            max_chunks: 16,
            max_duration: Duration::from_secs(10),
        },
    }
}

async fn fixture(store: Store) -> (StoreLayout<Store>, RepositorySnapshot, PointerDependency) {
    let layout = StoreLayout::new(store.clone(), "dependency-test".to_owned());
    let file_hash = *blake3::hash(b"").as_bytes();
    let mut writer = ShardWriter::new();
    writer
        .add_file(MDBFileInfo {
            metadata: FileDataSequenceHeader::new(MerkleHash::from(file_hash), 0, false, false),
            segments: vec![],
            verification: vec![],
            metadata_ext: None,
        })
        .unwrap();
    let (body, shard) = writer.finalize().unwrap();
    store
        .put(&layout.shard_path(&shard), Bytes::from(body))
        .await
        .unwrap();
    let (index_hash, _, index) = compact_shard_index(1, &[shard.hex()]).unwrap();
    manifest_store::upload_segmented_bulk(
        &store,
        &layout,
        &BulkData {
            shard_index: index,
            pack_index: Default::default(),
        },
    )
    .await
    .unwrap();
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    manifest.shard_index_hash = index_hash;
    manifest.seal_git_validation();
    manifest_store::create_manifest(&store, &layout, &manifest)
        .await
        .unwrap();
    let snapshot = manifest_store::read_repository_snapshot(&store, &layout)
        .await
        .unwrap();
    let PointerKind::Lfs(pointer) = lfs(5).pointer else {
        panic!("LFS pointer")
    };
    let path = crab_lfs::LfsObjectStore::object_path_for_prefix(layout.repo_prefix(), &pointer.oid);
    store
        .put(&path, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    (
        layout,
        snapshot,
        PointerDependency {
            blob: ObjectId::from_bytes_or_panic(&[1; 20]),
            pointer: PointerKind::Crab(Pointer {
                file_hash,
                size: 0,
                shard_hint: None,
            }),
        },
    )
}

#[tokio::test]
async fn mixed_batch_verifies_once_without_storage_mutation() {
    let storage = Arc::new(InMemory::new());
    let (layout, snapshot, crab) = fixture(Store::new(storage.clone())).await;
    let before = storage
        .list(None)
        .map_ok(|meta| (meta.location, (meta.e_tag, meta.size)))
        .try_collect::<BTreeMap<_, _>>()
        .await
        .unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&reads);
    let store = layout
        .store()
        .clone()
        .with_read_request_observer(Arc::new(move |kind| {
            if matches!(kind, crab_storage::StorageReadKind::Stream) {
                observed.fetch_add(1, Ordering::Relaxed);
            }
        }));
    let layout = StoreLayout::new(store, layout.repo_prefix().to_owned());
    verify_dependencies(
        &layout,
        &snapshot,
        &[crab, lfs(5), lfs(5)],
        limits(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let after = storage
        .list(None)
        .map_ok(|meta| (meta.location, (meta.e_tag, meta.size)))
        .try_collect::<BTreeMap<_, _>>()
        .await
        .unwrap();
    assert_eq!((after, reads.load(Ordering::Relaxed)), (before, 1));
}

#[tokio::test]
async fn invalid_batches_reject_before_any_origin_read() {
    let (layout, snapshot, _) = fixture(Store::new(Arc::new(InMemory::new()))).await;
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&reads);
    let store = layout
        .store()
        .clone()
        .with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
        }));
    let layout = StoreLayout::new(store, layout.repo_prefix().to_owned());
    for (dependencies, budget) in [
        (vec![lfs(5), lfs(6)], limits()),
        (
            vec![lfs(5)],
            DependencyProofLimits {
                max_dependencies: 0,
                ..limits()
            },
        ),
        (
            vec![lfs(5)],
            DependencyProofLimits {
                max_total_file_bytes: 4,
                ..limits()
            },
        ),
        (vec![lfs(1025)], limits()),
        (
            vec![PointerDependency {
                blob: ObjectId::null(gix_hash::Kind::Sha1),
                pointer: PointerKind::NotAPointer,
            }],
            limits(),
        ),
    ] {
        assert!(
            verify_dependencies(
                &layout,
                &snapshot,
                &dependencies,
                budget,
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
    }
    assert_eq!(reads.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn missing_crab_dependency_cannot_be_supplied_by_a_hint() {
    let (layout, snapshot, mut crab) = fixture(Store::new(Arc::new(InMemory::new()))).await;
    if let PointerKind::Crab(pointer) = &mut crab.pointer {
        pointer.file_hash = [9; 32];
        pointer.shard_hint = Some([8; 32]);
    }
    assert!(matches!(
        verify_dependencies(
            &layout,
            &snapshot,
            &[crab],
            limits(),
            &CancellationToken::new()
        )
        .await,
        Err(DependencyProofError::Invalid { .. })
    ));
}

#[tokio::test]
async fn lfs_corruption_and_missing_content_retain_blob_context() {
    let (layout, snapshot, _) = fixture(Store::new(Arc::new(InMemory::new()))).await;
    let PointerKind::Lfs(pointer) = lfs(5).pointer else {
        panic!("LFS")
    };
    let path = crab_lfs::LfsObjectStore::object_path_for_prefix(layout.repo_prefix(), &pointer.oid);
    layout.store().delete(&path).await.unwrap();
    layout
        .store()
        .put(&path, Bytes::from_static(b"wrong"))
        .await
        .unwrap();
    let corrupt = verify_dependencies(
        &layout,
        &snapshot,
        &[lfs(5)],
        limits(),
        &CancellationToken::new(),
    )
    .await;
    layout.store().delete(&path).await.unwrap();
    let missing = verify_dependencies(
        &layout,
        &snapshot,
        &[lfs(5)],
        limits(),
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        (corrupt, missing),
        (
            Err(DependencyProofError::Lfs {
                source: crab_lfs::LfsError::ObjectCorrupt { .. },
                ..
            }),
            Err(DependencyProofError::Lfs {
                source: crab_lfs::LfsError::ObjectMissing { .. },
                ..
            })
        )
    ));
}

#[tokio::test]
async fn batch_deadline_covers_pending_lfs_and_cancellation() {
    let throttled = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig::default(),
    ));
    let (layout, snapshot, _) = fixture(Store::new(throttled.clone())).await;
    throttled.config_mut(|config| config.wait_get_per_call = Duration::from_secs(5));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = verify_dependencies(&layout, &snapshot, &[lfs(5)], limits(), &cancel).await;
    let deadline = verify_dependencies(
        &layout,
        &snapshot,
        &[lfs(5)],
        DependencyProofLimits {
            max_duration: Duration::from_millis(10),
            ..limits()
        },
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        (cancelled, deadline),
        (
            Err(DependencyProofError::Cancelled),
            Err(DependencyProofError::Deadline)
        )
    ));
}
