use super::*;

async fn populated_catalog(store: crab_storage::Store) -> CatalogStore {
    let catalog = CatalogStore::new(StorageRoot::memory(store, "repositories"));
    catalog
        .create_repository(
            "team".into(),
            "project".into(),
            "team/project".into(),
            "main".into(),
            String::new(),
            vec![admin()],
        )
        .await
        .unwrap();
    catalog.load().await.unwrap();
    catalog
}

#[tokio::test]
async fn unchanged_membership_revalidates_without_retransmitting_the_catalog() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let gets = Arc::new(AtomicU64::new(0));
    let heads = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let observed_gets = gets.clone();
    let observed_heads = heads.clone();
    let observed_bytes = bytes.clone();
    let store = crab_storage::Store::new(Arc::new(InMemory::new()))
        .with_read_request_observer(Arc::new(move |kind| match kind {
            crab_storage::StorageReadKind::Get => {
                observed_gets.fetch_add(1, Ordering::Relaxed);
            }
            crab_storage::StorageReadKind::Head => {
                observed_heads.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }))
        .with_read_byte_observer(Arc::new(move |count| {
            observed_bytes.fetch_add(count, Ordering::Relaxed);
        }));
    let catalog = populated_catalog(store).await;
    gets.store(0, Ordering::Relaxed);
    heads.store(0, Ordering::Relaxed);
    bytes.store(0, Ordering::Relaxed);
    catalog.membership("team", "project").await.unwrap();
    assert_eq!(
        (
            heads.load(Ordering::Relaxed),
            gets.load(Ordering::Relaxed),
            bytes.load(Ordering::Relaxed),
        ),
        (1, 0, 0)
    );
}

#[tokio::test]
async fn revalidated_membership_observes_an_independent_writers_revocation() {
    let catalog = populated_catalog(crab_storage::Store::new(Arc::new(InMemory::new()))).await;
    let other = CatalogStore::new(catalog.root.clone());
    other
        .set_members("team", "project", vec![], false)
        .await
        .unwrap();
    assert!(
        catalog
            .membership("team", "project")
            .await
            .unwrap()
            .members
            .is_empty()
    );
}

#[tokio::test]
async fn revalidated_catalog_refuses_corruption_instead_of_retaining_membership() {
    let catalog = populated_catalog(crab_storage::Store::new(Arc::new(InMemory::new()))).await;
    catalog
        .root
        .store
        .put_overwrite(&catalog.path, Bytes::from_static(b"corrupt catalog"))
        .await
        .unwrap();
    assert!(matches!(
        catalog.membership("team", "project").await,
        Err(CatalogError::Json(_))
    ));
}

#[tokio::test]
async fn revalidated_catalog_does_not_retain_a_deleted_repository() {
    let catalog = populated_catalog(crab_storage::Store::new(Arc::new(InMemory::new()))).await;
    catalog.root.store.delete(&catalog.path).await.unwrap();
    assert!(matches!(
        catalog.membership("team", "project").await,
        Err(CatalogError::NotFound)
    ));
}
