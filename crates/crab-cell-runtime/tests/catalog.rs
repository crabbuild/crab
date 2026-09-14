use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use crab_cell_runtime::{
    ApplicationId, CatalogEntry, CatalogRole, CellAuthority, CellCatalog, CellTarget, Digest,
    IncarnationId, NamespaceId, Owner, SessionId, TenantId,
};
use crab_storage::{CellStorageLayout, Store};
use object_store::{memory::InMemory, path::Path};

fn fixture() -> (CellStorageLayout, CellCatalog, CellTarget) {
    let tenant = TenantId::from_bytes([1; 16]);
    let application = ApplicationId::from_bytes([2; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
        *application.as_bytes(),
    );
    let catalog = CellCatalog::new(layout.clone(), tenant);
    let target = CellTarget::new(
        tenant,
        application,
        NamespaceId::from_bytes([3; 16]),
        b"repository-1",
    )
    .unwrap();
    (layout, catalog, target)
}

fn entry(target: &CellTarget, role: CatalogRole, byte: u8) -> CatalogEntry {
    CatalogEntry::new(target, role, Digest::from_bytes([byte; 32]), 1).unwrap()
}

#[tokio::test]
async fn catalog_provision_is_idempotent_and_precedes_control() {
    let (layout, catalog, target) = fixture();
    let expected = entry(&target, CatalogRole::Repository, 4);
    let first = catalog.provision(expected.clone()).await.unwrap();
    let second = catalog.provision(expected.clone()).await.unwrap();
    assert_eq!(first.revision(), 1);
    assert_eq!(second.revision(), 1);
    assert_eq!(first.entry(), &expected);
    assert_eq!(
        catalog
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .entry(),
        &expected
    );
    let authority = CellAuthority::new(layout);
    assert!(authority.load(target.cell_id()).await.unwrap().is_none());
    let owner = Owner {
        session: SessionId::from_bytes([10; 16]),
        endpoint: "https://node.internal:8081".into(),
    };
    let created = authority
        .create_initial(&first, IncarnationId::from_bytes([11; 16]), owner.clone())
        .await
        .unwrap();
    let adopted = authority
        .create_initial(&first, IncarnationId::from_bytes([11; 16]), owner)
        .await
        .unwrap();
    assert_eq!(created.value(), adopted.value());
    assert!(matches!(
        authority
            .create_initial(
                &first,
                IncarnationId::from_bytes([11; 16]),
                Owner {
                    session: SessionId::from_bytes([12; 16]),
                    endpoint: "https://other.internal:8081".into(),
                },
            )
            .await,
        Err(crab_cell_runtime::Error::CellAlreadyActive)
    ));
}

#[tokio::test]
async fn concurrent_catalog_writers_merge_entries_on_one_shard() {
    let (_, catalog, first, second) = same_shard_targets();
    let first_entry = entry(&first, CatalogRole::Repository, 5);
    let second_entry = entry(&second, CatalogRole::Repository, 6);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let first_writer = {
        let catalog = catalog.clone();
        let barrier = barrier.clone();
        let entry = first_entry.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            catalog.provision(entry).await
        })
    };
    let second_writer = {
        let catalog = catalog.clone();
        let entry = second_entry.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            catalog.provision(entry).await
        })
    };
    first_writer.await.unwrap().unwrap();
    second_writer.await.unwrap().unwrap();

    assert_eq!(
        catalog
            .lookup(first.cell_id())
            .await
            .unwrap()
            .unwrap()
            .entry(),
        &first_entry
    );
    assert_eq!(
        catalog
            .lookup(second.cell_id())
            .await
            .unwrap()
            .unwrap()
            .entry(),
        &second_entry
    );
}

#[tokio::test]
async fn catalog_rejects_conflicting_bootstrap_contract_for_one_cell() {
    let (_, catalog, target) = fixture();
    catalog
        .provision(entry(&target, CatalogRole::Repository, 7))
        .await
        .unwrap();
    assert!(matches!(
        catalog.provision(entry(&target, CatalogRole::Sql, 8)).await,
        Err(crab_cell_runtime::Error::CatalogCollision)
    ));
}

#[tokio::test]
async fn catalog_page_digest_is_checked_before_entry_use() {
    let (layout, catalog, target) = fixture();
    catalog
        .provision(entry(&target, CatalogRole::Repository, 9))
        .await
        .unwrap();
    let (head, _) = layout
        .store()
        .get_with_etag_bounded(
            &layout.catalog_head_path(target.cell_id().as_bytes()[0]),
            32 * 1024,
        )
        .await
        .unwrap();
    let head: serde_json::Value = serde_json::from_slice(&head).unwrap();
    let digest = decode_digest(head["pages"][0].as_str().unwrap());
    layout
        .store()
        .put_overwrite(
            &layout.catalog_object_path(&digest),
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert!(matches!(
        catalog.lookup(target.cell_id()).await,
        Err(crab_cell_runtime::Error::Catalog("page digest mismatch"))
    ));
}

fn same_shard_targets() -> (CellStorageLayout, CellCatalog, CellTarget, CellTarget) {
    let tenant = TenantId::from_bytes([10; 16]);
    let application = ApplicationId::from_bytes([11; 16]);
    let namespace = NamespaceId::from_bytes([12; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
        *application.as_bytes(),
    );
    let catalog = CellCatalog::new(layout.clone(), tenant);
    let mut targets = HashMap::new();
    for partition in 0_u32..=256 {
        let target =
            CellTarget::new(tenant, application, namespace, &partition.to_be_bytes()).unwrap();
        let shard = target.cell_id().as_bytes()[0];
        if let Some(first) = targets.insert(shard, target.clone()) {
            return (layout, catalog, first, target);
        }
    }
    panic!("257 targets must contain a first-byte collision");
}

fn decode_digest(value: &str) -> [u8; 32] {
    let mut digest = [0; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    digest
}

fn nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("catalog digest must be lowercase hex"),
    }
}
