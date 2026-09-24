use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use object_store::{ObjectStoreExt, memory::InMemory, path::Path};

use super::*;
use crate::node::log::{RecoveryBase, build_recovery_overlays};

struct RecoveryFixture {
    inner: Arc<InMemory>,
    layout: CellStorageLayout,
    replica: crab_ltx::CellReplica,
    manifests: RecoveryManifestStore,
    pinned: PinnedRecoveryCell,
    publication: RecoveryPublicationSummary,
    base: crab_ltx::RootRef,
    final_position: crab_ltx::Position,
}

async fn recovery_fixture() -> RecoveryFixture {
    recovery_fixture_with_store(None).await
}

struct MemoryArtifactStore {
    limits: crab_ltx::Limits,
    reject_retain: bool,
    bundles: Mutex<BTreeMap<RecoveryArtifactKey, Vec<u8>>>,
    loads: AtomicU64,
}

struct MemoryArtifactLease;

impl crab_ltx::bundle::BundleLease for MemoryArtifactLease {}

impl RecoveryArtifactStore for MemoryArtifactStore {
    fn retain(&self, key: RecoveryArtifactKey, bundle: crab_ltx::bundle::Bundle) -> Result<()> {
        if self.reject_retain {
            return Err(Error::Capacity("artifact test store"));
        }
        let bytes = bundle.read_all()?;
        self.bundles
            .lock()
            .map_err(|_| Error::Node("artifact test store lock poisoned"))?
            .insert(key, bytes.to_vec());
        Ok(())
    }

    fn load(&self, key: &RecoveryArtifactKey) -> Result<Option<RecoveryArtifact>> {
        let bytes = self
            .bundles
            .lock()
            .map_err(|_| Error::Node("artifact test store lock poisoned"))?
            .get(key)
            .cloned();
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        self.loads.fetch_add(1, Ordering::Relaxed);
        let bundle = crab_ltx::bundle::Bundle::decode(bytes, self.limits)?;
        Ok(Some(RecoveryArtifact::new(
            bundle,
            Arc::new(MemoryArtifactLease),
        )))
    }
}

async fn recovery_fixture_with_store(
    artifacts: Option<Arc<dyn RecoveryArtifactStore>>,
) -> RecoveryFixture {
    let limits = crab_ltx::Limits::default();
    let directory = tempfile::TempDir::new().unwrap();
    let mut database = crab_ltx::Db::open(&directory.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let first = database.capture().unwrap();
    let inner = Arc::new(InMemory::new());
    let store = crab_storage::Store::new(inner.clone());
    let application = [3; 16];
    let cell = [4; 32];
    let incarnation = [5; 16];
    let layout = CellStorageLayout::new(store, Path::from("root"), application);
    let replica = crab_ltx::CellReplica::new(layout.clone(), cell, incarnation, limits).unwrap();
    let base = replica.prepare(None, &first, 1, 1).await.unwrap().root();
    database
        .transaction(|transaction| {
            transaction.execute("INSERT INTO values_ VALUES (1)", [])?;
            Ok(())
        })
        .unwrap();
    let tail = database.capture().unwrap();
    let segment = tail.segments.first().unwrap();
    let frame = crab_ltx::encode_node_frame(
        crab_ltx::NodeFrameScope {
            leader_session: [1; 16],
            log_epoch: 2,
            node_sequence: 1,
            application,
            cell,
            incarnation,
            cell_epoch: 6,
            commit_sequence: 2,
        },
        segment.info().clone(),
        Bytes::from(std::fs::read(segment.path()).unwrap()),
        limits,
    )
    .unwrap();
    let recovered = build_recovery_overlays(
        vec![frame],
        &[RecoveryBase {
            application,
            cell_epoch: 6,
            root: base,
        }],
        limits,
    )
    .unwrap();
    let manifests = RecoveryManifestStore::new(layout.clone(), limits);
    let manifests = artifacts.map_or(manifests.clone(), |store| {
        manifests.with_recovery_artifacts(store)
    });
    let pinned = manifests
        .pin_with_summary(SessionId::from_bytes([1; 16]), 2, recovered)
        .await
        .unwrap();
    let publication = pinned.summary;
    let mut pinned = pinned.cells;
    database.close().unwrap();
    RecoveryFixture {
        inner,
        layout,
        replica,
        manifests,
        pinned: pinned.pop().unwrap(),
        publication,
        base,
        final_position: tail.position,
    }
}

async fn load_error(fixture: &RecoveryFixture, recovery: &RecoveryOverlayRef) -> Error {
    match fixture
        .manifests
        .load_overlay(fixture.pinned.cell, fixture.pinned.incarnation, recovery)
        .await
    {
        Ok(_) => panic!("corrupt recovery input must not load"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn pinned_manifest_reopens_exact_overlay_and_prepares_successor() {
    let fixture = recovery_fixture().await;
    assert!(fixture.publication.bundle_bytes > 0);
    assert!(fixture.publication.object_reads > 0);
    assert!(fixture.publication.object_writes > 0);
    let overlay = fixture
        .manifests
        .load_overlay(
            fixture.pinned.cell,
            fixture.pinned.incarnation,
            &fixture.pinned.recovery,
        )
        .await
        .unwrap();
    let prepared = fixture
        .replica
        .prepare_recovered_overlay(&overlay, 1)
        .await
        .unwrap();
    assert_eq!(prepared.predecessor(), Some(fixture.base));
    assert_eq!(prepared.root().position, fixture.final_position);
    let mut control = crate::control::Control::initial(
        fixture.pinned.cell,
        fixture.pinned.incarnation,
        crate::control::Owner {
            session: SessionId::from_bytes([1; 16]),
            endpoint: "https://dead.internal:8081".into(),
        },
        Digest::from_bytes([12; 32]),
        1,
    )
    .unwrap();
    control.state = crate::control::ControlState::Serving;
    control.root = Some(runtime_root(fixture.base));
    let attached = control
        .attach_recovery(fixture.pinned.recovery.clone())
        .unwrap();
    let takeover = attached
        .takeover(crate::control::Owner {
            session: SessionId::from_bytes([13; 16]),
            endpoint: "https://successor.internal:8081".into(),
        })
        .unwrap();
    let published = takeover.publish_recovery(&prepared, None).unwrap();
    assert_eq!(published.state, crate::control::ControlState::Recovering);
    assert!(published.recovery.is_none());
    assert_eq!(published.root.unwrap().txid, fixture.final_position.txid);
}

#[tokio::test]
async fn verified_artifact_store_hit_reuses_the_pinned_bundle() {
    let limits = crab_ltx::Limits::default();
    let artifacts = Arc::new(MemoryArtifactStore {
        limits,
        reject_retain: false,
        bundles: Mutex::new(BTreeMap::new()),
        loads: AtomicU64::new(0),
    });
    let fixture = recovery_fixture_with_store(Some(
        Arc::clone(&artifacts) as Arc<dyn RecoveryArtifactStore>
    ))
    .await;
    let overlay = fixture
        .manifests
        .load_overlay(
            fixture.pinned.cell,
            fixture.pinned.incarnation,
            &fixture.pinned.recovery,
        )
        .await
        .unwrap();
    assert_eq!(artifacts.loads.load(Ordering::Relaxed), 1);
    let prepared = fixture
        .replica
        .prepare_recovered_overlay(&overlay, 1)
        .await
        .unwrap();
    assert_eq!(prepared.root().position, fixture.final_position);
}

#[tokio::test]
async fn artifact_cache_failure_keeps_object_store_recovery_available() {
    let artifacts = Arc::new(MemoryArtifactStore {
        limits: crab_ltx::Limits::default(),
        reject_retain: true,
        bundles: Mutex::new(BTreeMap::new()),
        loads: AtomicU64::new(0),
    });
    let fixture = recovery_fixture_with_store(Some(
        Arc::clone(&artifacts) as Arc<dyn RecoveryArtifactStore>
    ))
    .await;
    let overlay = fixture
        .manifests
        .load_overlay(
            fixture.pinned.cell,
            fixture.pinned.incarnation,
            &fixture.pinned.recovery,
        )
        .await
        .unwrap();
    assert_eq!(artifacts.loads.load(Ordering::Relaxed), 0);
    assert_eq!(overlay.final_position(), fixture.final_position);
}

#[tokio::test]
async fn loaded_overlay_holds_bundle_disk_reservation_until_drop() {
    let fixture = recovery_fixture().await;
    let scratch = tempfile::TempDir::new().unwrap();
    let recovery = &fixture.pinned.recovery;
    let manifest_path = fixture.layout.node_log_recovery_path(
        recovery.leader_session.as_bytes(),
        recovery.log_epoch,
        recovery.manifest_digest.as_bytes(),
    );
    let (body, _) = fixture
        .layout
        .store()
        .get_with_etag_bounded(&manifest_path, MAX_MANIFEST_BYTES)
        .await
        .unwrap();
    let manifest = RecoveryManifest::decode(&body).unwrap();
    let bundle_path = fixture.layout.node_log_bundle_path(
        recovery.leader_session.as_bytes(),
        recovery.log_epoch,
        &manifest.cells[0].bundle_digest,
    );
    let size = fixture
        .layout
        .store()
        .head(&bundle_path)
        .await
        .unwrap()
        .size;
    let budget = crab_ltx::DiskBudget::new(size);
    let manifests = RecoveryManifestStore::new(fixture.layout.clone(), crab_ltx::Limits::default())
        .with_recovery_disk(budget.clone())
        .with_recovery_scratch(scratch.path().to_owned());
    let overlay = manifests
        .load_overlay(fixture.pinned.cell, fixture.pinned.incarnation, recovery)
        .await
        .unwrap();
    assert_eq!(overlay.bundle().len(), size);
    assert_eq!(budget.used(), size);
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 1);
    drop(overlay);
    assert_eq!(budget.used(), 0);
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn load_overlay_rejects_corrupt_manifest_bytes() {
    let fixture = recovery_fixture().await;
    let recovery = &fixture.pinned.recovery;
    let path = fixture.layout.node_log_recovery_path(
        recovery.leader_session.as_bytes(),
        recovery.log_epoch,
        recovery.manifest_digest.as_bytes(),
    );
    fixture
        .inner
        .put(&path, Bytes::from_static(b"corrupt manifest").into())
        .await
        .unwrap();

    let error = load_error(&fixture, recovery).await;

    assert!(matches!(
        error,
        Error::Node("recovery manifest digest differs")
    ));
}

#[tokio::test]
async fn load_overlay_rejects_self_consistent_manifest_metadata_change() {
    let fixture = recovery_fixture().await;
    let mut recovery = fixture.pinned.recovery.clone();
    let original_path = fixture.layout.node_log_recovery_path(
        recovery.leader_session.as_bytes(),
        recovery.log_epoch,
        recovery.manifest_digest.as_bytes(),
    );
    let (body, _) = fixture
        .layout
        .store()
        .get_with_etag_bounded(&original_path, MAX_MANIFEST_BYTES)
        .await
        .unwrap();
    let mut raw: RawManifest = serde_json::from_slice(&body).unwrap();
    raw.cells[0].final_commit_sequence = "3".into();
    let changed = serde_json::to_vec(&raw).unwrap();
    let digest = *blake3::hash(&changed).as_bytes();
    let changed_path = fixture.layout.node_log_recovery_path(
        recovery.leader_session.as_bytes(),
        recovery.log_epoch,
        &digest,
    );
    fixture
        .layout
        .store()
        .put(&changed_path, Bytes::from(changed))
        .await
        .unwrap();
    recovery.manifest_digest = Digest::from_bytes(digest);

    let error = load_error(&fixture, &recovery).await;

    assert!(matches!(
        error,
        Error::Node("recovery control pointer differs from manifest")
    ));
}

#[tokio::test]
async fn load_overlay_rejects_corrupt_bundle_bytes() {
    let fixture = recovery_fixture().await;
    let recovery = &fixture.pinned.recovery;
    let manifest_path = fixture.layout.node_log_recovery_path(
        recovery.leader_session.as_bytes(),
        recovery.log_epoch,
        recovery.manifest_digest.as_bytes(),
    );
    let (body, _) = fixture
        .layout
        .store()
        .get_with_etag_bounded(&manifest_path, MAX_MANIFEST_BYTES)
        .await
        .unwrap();
    let manifest = RecoveryManifest::decode(&body).unwrap();
    let bundle_path = fixture.layout.node_log_bundle_path(
        recovery.leader_session.as_bytes(),
        recovery.log_epoch,
        &manifest.cells[0].bundle_digest,
    );
    fixture
        .inner
        .put(&bundle_path, Bytes::from_static(b"corrupt bundle").into())
        .await
        .unwrap();

    let error = load_error(&fixture, recovery).await;

    assert!(matches!(
        error,
        Error::Node("recovery bundle digest differs")
    ));
}
