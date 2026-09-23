use std::{sync::Arc, time::UNIX_EPOCH};

use bytes::Bytes;
use crab_ltx::CellObjectKind;
use crab_storage::{ObjectStoreCredentials, Store, build_explicit_store};
use object_store::{memory::InMemory, path::Path};

use super::*;
use crate::cell::catalog::CatalogEntry;
use crate::cell::catalog::CatalogRole;
use crate::control::{Owner, RootRef};
use crate::identity::IncarnationId;
use crate::identity::{
    ApplicationId, CellId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};

fn identity() -> ApplicationIdentity {
    ApplicationIdentity::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
    )
}

fn target(identity: ApplicationIdentity, partition: &[u8]) -> CellTarget {
    CellTarget::new(
        identity.tenant(),
        identity.application(),
        NamespaceId::from_bytes([3; 16]),
        partition,
    )
    .unwrap()
}

async fn root(
    layout: &CellStorageLayout,
    target: &CellTarget,
    incarnation: IncarnationId,
    payload: usize,
) -> (crab_ltx::RootRef, Vec<Path>) {
    let limits = ReplicaLimits::default();
    let replica = crab_ltx::CellReplica::new(
        layout.clone(),
        *target.cell_id().as_bytes(),
        *incarnation.as_bytes(),
        limits,
    )
    .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let mut database = crab_ltx::Db::open(&directory.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute(
                "CREATE TABLE values_(value BLOB NOT NULL)",
                rusqlite::params![],
            )?;
            transaction.execute(
                "INSERT INTO values_ VALUES(zeroblob(?1))",
                rusqlite::params![payload],
            )?;
            Ok(())
        })
        .unwrap();
    let prepared = replica
        .prepare(None, &database.capture().unwrap(), 1, 1)
        .await
        .unwrap();
    database.close().unwrap();
    let root = prepared.root();
    let paths = replica
        .reachable_objects(&root)
        .await
        .unwrap()
        .into_iter()
        .map(|object| {
            layout.incarnation_object_path(
                target.cell_id().as_bytes(),
                incarnation.as_bytes(),
                &object.digest,
                object.kind,
            )
        })
        .collect();
    (root, paths)
}

fn idle_control(
    cell: CellId,
    incarnation: IncarnationId,
    root: crab_ltx::RootRef,
    code: Digest,
) -> Control {
    let control = Control {
        cell,
        incarnation,
        epoch: 1,
        revision: 3,
        progress: 3,
        state: ControlState::Idle,
        owner: None,
        root: Some(RootRef::from_ltx(cell, incarnation, root).unwrap()),
        recovery: None,
        code,
        schema: 1,
        next_due_ms: None,
    };
    control.encode().unwrap();
    control
}

async fn pinned_catalog(catalog: &CellCatalog) -> Vec<crate::recovery::backup::PinnedCatalogShard> {
    let mut pinned = Vec::with_capacity(256);
    for shard in 0_u8..=u8::MAX {
        let mut scan = catalog.scan_shard(shard).await.unwrap();
        let revision = scan.revision();
        let pages = scan.page_digests().to_vec();
        while scan.next_page().await.unwrap().is_some() {}
        pinned.push(crate::recovery::backup::PinnedCatalogShard {
            shard,
            revision,
            pages,
        });
    }
    pinned
}

async fn put_content_addressed(layout: &CellStorageLayout, path: Path, body: &[u8]) -> Path {
    layout
        .store()
        .put(&path, Bytes::copy_from_slice(body))
        .await
        .unwrap();
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn maintenance_collection_preserves_live_and_pinned_graphs() {
    collection_preserves_live_and_pinned_graphs(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated pre-created RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_maintenance_collection_preserves_live_and_pinned_graphs() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let store = build_explicit_store(
        &required("CRAB_CELL_TEST_BUCKET"),
        ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_CELL_TEST_ENDPOINT")),
        true,
    )
    .unwrap();
    collection_preserves_live_and_pinned_graphs(
        store,
        Path::from(format!("{}/retention", required("CRAB_CELL_TEST_PREFIX"))),
    )
    .await;
}

async fn collection_preserves_live_and_pinned_graphs(store: Store, prefix: Path) {
    let identity = identity();
    let layout = CellStorageLayout::new(store, prefix, *identity.application().as_bytes());
    let catalog = CellCatalog::new(layout.clone(), identity.tenant());
    let code = Digest::from_bytes([4; 32]);
    let current_target = target(identity, b"current");
    let pinned_target = target(identity, b"pinned");
    for target in [&current_target, &pinned_target] {
        catalog
            .provision(CatalogEntry::new(target, CatalogRole::Repository, code, 1).unwrap())
            .await
            .unwrap();
    }

    let current_incarnation = IncarnationId::from_bytes([5; 16]);
    let pinned_incarnation = IncarnationId::from_bytes([6; 16]);
    let orphan_incarnation = IncarnationId::from_bytes([7; 16]);
    let (current_root, current_paths) =
        root(&layout, &current_target, current_incarnation, 128_000).await;
    let (pinned_root, pinned_paths) =
        root(&layout, &pinned_target, pinned_incarnation, 96_000).await;
    let orphan_target = target(identity, b"orphan");
    let (_, orphan_paths) = root(&layout, &orphan_target, orphan_incarnation, 64_000).await;
    let current = idle_control(
        current_target.cell_id(),
        current_incarnation,
        current_root,
        code,
    );
    let pinned = idle_control(
        pinned_target.cell_id(),
        pinned_incarnation,
        pinned_root,
        code,
    );

    let descriptor = br#"{"runtime":"retention-test","version":1}"#;
    let descriptor_digest = Digest::from_bytes(*blake3::hash(descriptor).as_bytes());
    let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
    let initial_operation = RequestId::from_bytes([8; 16]);
    let prepared = releases
        .prepare(
            descriptor,
            descriptor_digest,
            0,
            &format!("sha256:{}", "a".repeat(64)),
            initial_operation,
        )
        .await
        .unwrap();
    let activating = releases
        .start_activation(prepared.revision(), initial_operation)
        .await
        .unwrap();
    let ready = releases
        .complete_activation(activating.revision(), initial_operation)
        .await
        .unwrap();

    let pins = BackupPinStore::new(
        layout.clone(),
        identity,
        ReplicaLimits::default(),
        ReplicaHost::default(),
    )
    .unwrap();
    pins.create(
        RequestId::from_bytes([9; 16]),
        1_000,
        pinned_catalog(&catalog).await,
        vec![current.clone(), pinned.clone()],
    )
    .await
    .unwrap();

    layout
        .store()
        .create_strict(
            &layout.control_path(current.cell.as_bytes()),
            Bytes::from(current.encode().unwrap()),
        )
        .await
        .unwrap();
    let mut tombstoned = pinned;
    tombstoned.epoch += 1;
    tombstoned.revision += 1;
    tombstoned.progress += 1;
    tombstoned.state = ControlState::Tombstoned;
    tombstoned.encode().unwrap();
    layout
        .store()
        .create_strict(
            &layout.control_path(tombstoned.cell.as_bytes()),
            Bytes::from(tombstoned.encode().unwrap()),
        )
        .await
        .unwrap();

    let orphan_release_body = b"orphan release descriptor";
    let orphan_release_digest = blake3::hash(orphan_release_body);
    let orphan_release = put_content_addressed(
        &layout,
        layout.release_descriptor_path(orphan_release_digest.as_bytes()),
        orphan_release_body,
    )
    .await;
    let orphan_catalog_body = b"orphan catalog page";
    let orphan_catalog_digest = blake3::hash(orphan_catalog_body);
    let orphan_catalog = put_content_addressed(
        &layout,
        layout.catalog_object_path(orphan_catalog_digest.as_bytes()),
        orphan_catalog_body,
    )
    .await;
    let orphan_pin_body = b"orphan pin object";
    let orphan_pin_digest = blake3::hash(orphan_pin_body);
    let orphan_pin = put_content_addressed(
        &layout,
        layout.pin_object_path(orphan_pin_digest.as_bytes()),
        orphan_pin_body,
    )
    .await;
    let unknown = put_content_addressed(
        &layout,
        Path::from(format!(
            "{}/releases/future-format.bin",
            layout.application_prefix()
        )),
        b"future",
    )
    .await;

    let maintenance_operation = RequestId::from_bytes([10; 16]);
    let prepared = releases
        .prepare(
            descriptor,
            descriptor_digest,
            ready.revision(),
            &format!("sha256:{}", "a".repeat(64)),
            maintenance_operation,
        )
        .await
        .unwrap();
    let maintenance = releases
        .start_maintenance(prepared.revision(), maintenance_operation)
        .await
        .unwrap();
    assert!(
        pins.create(
            RequestId::from_bytes([11; 16]),
            2_000,
            pinned_catalog(&catalog).await,
            vec![current.clone(), tombstoned.clone()],
        )
        .await
        .is_err()
    );

    let collector = CellGarbageCollector::new(
        layout.clone(),
        identity,
        ReplicaLimits::default(),
        ReplicaHost::default(),
    )
    .unwrap();
    let scratch = tempfile::TempDir::new().unwrap();
    let grace = collector
        .collect(
            &maintenance,
            scratch.path(),
            GarbageCollectionPolicy::new(0, 1, 100_000).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(grace.deleted_objects(), 0);
    assert!(grace.grace_objects() >= orphan_paths.len() as u64 + 3);

    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let limited = collector
        .collect(
            &maintenance,
            scratch.path(),
            GarbageCollectionPolicy::new(now_ms.saturating_add(60_000), 1, 1).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(limited.deleted_objects(), 1);
    assert!(limited.eligible_objects() > limited.deleted_objects());
    assert!(!limited.complete());

    let report = collector
        .collect(
            &maintenance,
            scratch.path(),
            GarbageCollectionPolicy::new(now_ms.saturating_add(60_000), 1, 100_000).unwrap(),
        )
        .await
        .unwrap();
    assert!(report.complete());
    assert_eq!(report.current_controls(), 2);
    assert_eq!(report.retained_pins(), 1);
    assert!(limited.deleted_objects() + report.deleted_objects() >= orphan_paths.len() as u64 + 3);
    assert!(report.reachable_objects() >= current_paths.len() as u64 + pinned_paths.len() as u64);

    for path in current_paths.into_iter().chain(pinned_paths) {
        layout.store().head(&path).await.unwrap();
    }
    for path in orphan_paths
        .into_iter()
        .chain([orphan_release, orphan_catalog, orphan_pin])
    {
        assert!(matches!(
            layout.store().head(&path).await,
            Err(StorageError::NotFound { .. })
        ));
    }
    layout.store().head(&unknown).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn maintenance_collection_rejects_an_owned_current_cell_before_deleting() {
    let identity = identity();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
        *identity.application().as_bytes(),
    );
    let code = Digest::from_bytes([12; 32]);
    let target = target(identity, b"owned");
    CellCatalog::new(layout.clone(), identity.tenant())
        .provision(CatalogEntry::new(&target, CatalogRole::Repository, code, 1).unwrap())
        .await
        .unwrap();
    let control = Control::initial(
        target.cell_id(),
        IncarnationId::from_bytes([13; 16]),
        Owner {
            session: SessionId::from_bytes([14; 16]),
            endpoint: "https://owner.internal:8789".into(),
        },
        code,
        1,
    )
    .unwrap();
    layout
        .store()
        .create_strict(
            &layout.control_path(control.cell.as_bytes()),
            Bytes::from(control.encode().unwrap()),
        )
        .await
        .unwrap();

    let orphan_body = b"old unreachable descriptor";
    let orphan_digest = blake3::hash(orphan_body);
    let orphan = put_content_addressed(
        &layout,
        layout.release_descriptor_path(orphan_digest.as_bytes()),
        orphan_body,
    )
    .await;
    let descriptor = br#"{"runtime":"retention-owner-test","version":1}"#;
    let descriptor_digest = Digest::from_bytes(*blake3::hash(descriptor).as_bytes());
    let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
    let operation = RequestId::from_bytes([15; 16]);
    let prepared = releases
        .prepare(
            descriptor,
            descriptor_digest,
            0,
            &format!("sha256:{}", "b".repeat(64)),
            operation,
        )
        .await
        .unwrap();
    let maintenance = releases
        .start_maintenance(prepared.revision(), operation)
        .await
        .unwrap();
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let scratch = tempfile::TempDir::new().unwrap();
    let error = CellGarbageCollector::new(
        layout.clone(),
        identity,
        ReplicaLimits::default(),
        ReplicaHost::default(),
    )
    .unwrap()
    .collect(
        &maintenance,
        scratch.path(),
        GarbageCollectionPolicy::new(now_ms.saturating_add(60_000), 1, 100).unwrap(),
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        Error::Retention("a current Cell still has an owner during maintenance")
    ));
    layout.store().head(&orphan).await.unwrap();
}

#[test]
fn immutable_path_classifier_accepts_only_version_one_layouts() {
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
        [2; 16],
    );
    let prefix = layout.application_prefix();
    assert!(immutable_candidate(
        &prefix,
        &layout.incarnation_object_path(&[3; 32], &[4; 16], &[5; 32], CellObjectKind::Ltx),
    ));
    assert!(immutable_candidate(
        &prefix,
        &layout.catalog_object_path(&[6; 32]),
    ));
    assert!(!immutable_candidate(
        &prefix,
        &layout.control_path(&[3; 32]),
    ));
    assert!(!immutable_candidate(
        &prefix,
        &Path::from(format!("{prefix}/releases/future-format.bin")),
    ));

    assert!(GarbageCollectionPolicy::new(0, 0, 1).is_err());
    assert!(GarbageCollectionPolicy::new(0, 1, 0).is_err());
    assert!(GarbageCollectionPolicy::new(0, 1, 100_001).is_err());
}
