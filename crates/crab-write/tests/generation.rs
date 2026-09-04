use std::{sync::Arc, time::Duration};

use crab_coordination::{GIT_OBJECT_LOCATOR_RESOURCE, PushLock};
use crab_metadata::{
    manifest_store,
    manifests::{Manifest, PackManifestEntry},
};
use crab_storage::{Store, StoreLayout};
use crab_write::{WriteError, generation::maintain_catalog};
use futures_util::TryStreamExt;
use tokio_util::sync::CancellationToken;

const TTL: Duration = Duration::from_secs(60);

fn storage() -> (Store, StoreLayout<Store>) {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "generation-owner".to_owned());
    (store, layout)
}

#[tokio::test]
async fn superseded_sample_never_opens_the_catalog() {
    let (store, layout) = storage();
    let mut captured = Manifest::default_for_repo("refs/heads/main");
    captured.generation = 1;
    captured.pack_index_hash = "b".repeat(64);
    captured.seal_git_validation();
    let mut current = captured.clone();
    current.generation = 2;
    current.seal_git_validation();
    manifest_store::create_manifest(&store, &layout, &current)
        .await
        .unwrap();
    assert!(
        maintain_catalog(
            &store,
            &layout,
            &captured,
            &[],
            TTL,
            &CancellationToken::new()
        )
        .await
        .unwrap()
        .is_none()
    );
    let objects: Vec<_> = store.inner().list(None).try_collect().await.unwrap();
    assert!(
        objects
            .iter()
            .all(|object| !object.location.as_ref().contains("git_object_catalog_db/"))
    );
    let lease = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        GIT_OBJECT_LOCATOR_RESOURCE,
        TTL,
    )
    .await
    .expect("stale owner released its lease");
    lease.release().await.unwrap();
}

#[tokio::test]
async fn failed_publication_closes_writer_and_releases_lease_before_retry() {
    let (store, layout) = storage();
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    manifest.pack_index_hash = "b".repeat(64);
    manifest.seal_git_validation();
    manifest_store::create_manifest(&store, &layout, &manifest)
        .await
        .unwrap();
    let pack = PackManifestEntry {
        pack_id: "a".repeat(64),
        content_hash: "a".repeat(64),
        size: 100,
        object_count: 1,
        ref_tips: vec![],
    };
    let result = maintain_catalog(
        &store,
        &layout,
        &manifest,
        &[pack],
        TTL,
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(result, Err(WriteError::Storage(_))));
    let lease = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        GIT_OBJECT_LOCATOR_RESOURCE,
        TTL,
    )
    .await
    .expect("failed publisher released its lease");
    lease.release().await.unwrap();
    let (_, etag) = manifest_store::read_manifest(&store, &layout)
        .await
        .unwrap();
    manifest.generation = 2;
    manifest.pack_index_hash.clear();
    manifest.seal_git_validation();
    manifest_store::write_manifest_cas(&store, &layout, &manifest, &etag)
        .await
        .unwrap();
    let retry = tokio::time::timeout(
        Duration::from_secs(10),
        maintain_catalog(
            &store,
            &layout,
            &manifest,
            &[],
            TTL,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("closed writer permits retry")
    .unwrap()
    .unwrap();
    assert_eq!(retry.stats.object_count, 0);
}

#[tokio::test]
async fn cancellation_while_waiting_cannot_release_another_catalog_writer() {
    let (store, layout) = storage();
    let manifest = Manifest::default_for_repo("refs/heads/main");
    manifest_store::create_manifest(&store, &layout, &manifest)
        .await
        .unwrap();
    let lease = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        GIT_OBJECT_LOCATOR_RESOURCE,
        TTL,
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    let operation = maintain_catalog(&store, &layout, &manifest, &[], TTL, &cancel);
    let signal = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(operation, signal)
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(WriteError::Cancelled)));
    assert!(matches!(
        PushLock::acquire_internal(
            store.inner(),
            layout.repo_prefix(),
            GIT_OBJECT_LOCATOR_RESOURCE,
            TTL
        )
        .await,
        Err(crab_coordination::CoordinationError::PushLockHeld { .. })
    ));
    lease.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generation_advance_after_captured_read_is_reported_as_superseded() {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    let (store, layout) = storage();
    let manifest = Manifest::default_for_repo("refs/heads/main");
    let etag = manifest_store::create_manifest_with_etag(&store, &layout, &manifest)
        .await
        .unwrap();
    let captured = Arc::new(tokio::sync::Notify::new());
    let observed = Arc::clone(&captured);
    let first = AtomicBool::new(true);
    let (release, released) = mpsc::sync_channel(1);
    let released = Mutex::new(released);
    // Pause after origin bytes have been captured, before the owner can use them.
    let owner_store = store.clone().with_read_byte_observer(Arc::new(move |_| {
        if first.swap(false, Ordering::SeqCst) {
            observed.notify_one();
            tokio::task::block_in_place(|| {
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
            });
        }
    }));
    let owner_layout = layout.clone();
    let owner_manifest = manifest.clone();
    let owner = tokio::spawn(async move {
        maintain_catalog(
            &owner_store,
            &owner_layout,
            &owner_manifest,
            &[],
            TTL,
            &CancellationToken::new(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), captured.notified())
        .await
        .unwrap();
    let mut newer = manifest;
    newer.generation += 1;
    newer.seal_git_validation();
    manifest_store::write_manifest_cas(&store, &layout, &newer, &etag)
        .await
        .unwrap();
    release.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), owner)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_none()
    );
    let lease = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        GIT_OBJECT_LOCATOR_RESOURCE,
        TTL,
    )
    .await
    .expect("superseded publisher released its lease");
    lease.release().await.unwrap();
}
