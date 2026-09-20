use std::sync::Arc;

use crab_ltx::{CellObjectKind, CellStorageLayout};
use crab_storage::Store;
use object_store::{ObjectStoreExt as _, memory::InMemory, path::Path};

use super::*;
use crate::{
    ApplicationId, ApplicationIdentityStore, CatalogEntry, CatalogRole, CellAuthority, CellTarget,
    ControlState, Digest, IncarnationId, NamespaceId, Owner, ReleaseStore, SessionId, TenantId,
};

async fn pinned_catalog(catalog: &CellCatalog) -> Vec<PinnedCatalogShard> {
    let mut pinned = Vec::with_capacity(256);
    for shard in 0_u8..=u8::MAX {
        let mut scan = catalog.scan_shard(shard).await.unwrap();
        let revision = scan.revision();
        let pages = scan.page_digests().to_vec();
        while scan.next_page().await.unwrap().is_some() {}
        pinned.push(PinnedCatalogShard {
            shard,
            revision,
            pages,
        });
    }
    pinned
}

#[tokio::test]
async fn pin_verifies_roots_and_fails_closed_when_a_dependency_is_missing() {
    let backend = Arc::new(InMemory::new());
    let store = Store::new(backend.clone());
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
    );
    let layout = CellStorageLayout::new(
        store,
        Path::from("runtime"),
        *identity.application().as_bytes(),
    );
    let target = CellTarget::new(
        identity.tenant(),
        identity.application(),
        NamespaceId::from_bytes([3; 16]),
        b"repository",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([4; 16]);
    let limits = ReplicaLimits::default();
    let replica = crab_ltx::CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        limits,
    )
    .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let mut database =
        crab_ltx::ManagedDb::open(&directory.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE values_(value BLOB NOT NULL);\
                 INSERT INTO values_ VALUES(randomblob(100000))",
            )
        })
        .unwrap();
    let root = replica
        .prepare(None, &database.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    database.close().unwrap();

    let mut control = Control::initial(
        cell,
        incarnation,
        Owner {
            session: SessionId::from_bytes([5; 16]),
            endpoint: "https://node.internal:8789".to_owned(),
        },
        Digest::from_bytes([6; 32]),
        1,
    )
    .unwrap();
    control.revision = 2;
    control.progress = 2;
    control.state = ControlState::Serving;
    control.root = Some(crate::RootRef::from_ltx(cell, incarnation, root).unwrap());
    control.encode().unwrap();

    let catalog = CellCatalog::new(layout.clone(), identity.tenant());
    catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Repository,
                Digest::from_bytes([6; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let pinned = pinned_catalog(&catalog).await;
    let descriptor = br#"{"runtime":"test","version":1}"#;
    let descriptor_digest = Digest::from_bytes(*blake3::hash(descriptor).as_bytes());
    let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
    let operation = RequestId::from_bytes([9; 16]);
    let prepared = releases
        .prepare(
            descriptor,
            descriptor_digest,
            0,
            &format!("sha256:{}", "a".repeat(64)),
            operation,
        )
        .await
        .unwrap();
    let activating = releases
        .start_activation(prepared.revision(), operation)
        .await
        .unwrap();
    releases
        .complete_activation(activating.revision(), operation)
        .await
        .unwrap();

    let pins =
        BackupPinStore::new(layout.clone(), identity, limits, ReplicaHost::default()).unwrap();
    let id = RequestId::from_bytes([7; 16]);
    let pin = pins
        .create(id, 1_000, pinned.clone(), vec![control.clone()])
        .await
        .unwrap();
    assert_eq!(pin.control_count(), 1);
    assert_eq!(pins.load(id).await.unwrap(), Some(pin.clone()));
    assert_eq!(pins.controls(&pin).await.unwrap(), vec![control.clone()]);
    assert_eq!(pins.verify(&pin).await.unwrap(), vec![control.clone()]);
    assert_eq!(
        pins.create(id, 1_000, pinned.clone(), vec![control.clone()])
            .await
            .unwrap(),
        pin
    );

    let destination_root = Path::from("restored");
    let restored = pins.restore(&pin, destination_root.clone()).await.unwrap();
    assert_eq!(restored.application(), identity.application());
    assert_eq!(restored.pin(), id);
    assert_eq!(restored.control_count(), 1);
    assert!(restored.immutable_object_count() > 0);
    assert_eq!(restored.nonempty_catalog_shards(), 1);
    assert_eq!(
        pins.restore(&pin, destination_root.clone()).await.unwrap(),
        restored
    );

    let restored_store = Store::new(backend.clone());
    let restored_identities =
        ApplicationIdentityStore::new(restored_store.clone(), destination_root.clone());
    assert_eq!(restored_identities.load().await.unwrap(), Some(identity));
    let restored_layout = restored_identities.layout(identity).await.unwrap();
    let restored_control = CellAuthority::new(restored_layout.clone())
        .load(cell)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored_control.value().state, ControlState::Idle);
    assert_eq!(restored_control.value().owner, None);
    assert_eq!(restored_control.value().root, control.root);
    assert_eq!(
        ReleaseStore::new(restored_layout.clone(), identity)
            .unwrap()
            .load()
            .await
            .unwrap()
            .unwrap()
            .record(),
        releases.load().await.unwrap().unwrap().record()
    );

    let missing = replica
        .reachable_objects(&root)
        .await
        .unwrap()
        .into_iter()
        .find(|object| object.kind == CellObjectKind::Ltx)
        .unwrap();
    let missing_path = layout.incarnation_object_path(
        cell.as_bytes(),
        incarnation.as_bytes(),
        &missing.digest,
        missing.kind,
    );
    backend.delete(&missing_path).await.unwrap();
    let restored_pins =
        BackupPinStore::new(restored_layout, identity, limits, ReplicaHost::default()).unwrap();
    let restored_pin = restored_pins.load(id).await.unwrap().unwrap();
    assert_eq!(restored_pins.verify(&restored_pin).await.unwrap().len(), 1);
    let next = RequestId::from_bytes([8; 16]);
    assert!(
        pins.create(next, 2_000, pinned, vec![control])
            .await
            .is_err()
    );
    assert!(pins.load(next).await.unwrap().is_none());
    assert!(pins.verify(&pin).await.is_err());
}
