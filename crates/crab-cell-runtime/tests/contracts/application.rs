//! Application identity tests extracted from `src/application.rs`.

use std::sync::Arc;

use bytes::Bytes;
use crab_storage::Store;
use object_store::memory::InMemory;
use object_store::path::Path;

use crab_cell_runtime::*;

fn identity(byte: u8) -> ApplicationIdentity {
    ApplicationIdentity::new(
        TenantId::from_bytes([byte; 16]),
        ApplicationId::from_bytes([byte.wrapping_add(1); 16]),
    )
}

#[tokio::test]
async fn initialization_is_idempotent_and_rejects_another_application() {
    let store = Store::new(Arc::new(InMemory::new()));
    let identities = ApplicationIdentityStore::new(store, Path::from("root"));
    let first = identity(1);

    assert_eq!(identities.initialize(first).await.unwrap(), first);
    assert_eq!(identities.initialize(first).await.unwrap(), first);
    assert!(matches!(
        identities.initialize(identity(9)).await,
        Err(Error::Identity(_))
    ));
    assert_eq!(identities.load().await.unwrap(), Some(first));
    assert_eq!(
        identities.layout(first).await.unwrap().application_id(),
        first.application().as_bytes()
    );
}

#[tokio::test]
async fn identity_reader_rejects_noncanonical_or_unknown_fields() {
    let store = Store::new(Arc::new(InMemory::new()));
    let identities = ApplicationIdentityStore::new(store.clone(), Path::from("root"));
    store
        .create_strict(
            &CellStorageLayout::root_identity_path(&Path::from("root")),
            Bytes::from_static(
                br#"{ "application":"02020202020202020202020202020202","tenant":"01010101010101010101010101010101","version":1}"#,
            ),
        )
        .await
        .unwrap();
    assert!(matches!(identities.load().await, Err(Error::Identity(_))));

    let other = ApplicationIdentityStore::new(store.clone(), Path::from("other"));
    store
        .create_strict(
            &CellStorageLayout::root_identity_path(&Path::from("other")),
            Bytes::from_static(
                br#"{"application":"02020202020202020202020202020202","tenant":"01010101010101010101010101010101","unexpected":true,"version":1}"#,
            ),
        )
        .await
        .unwrap();
    assert!(matches!(other.load().await, Err(Error::Json(_))));
}
