#![cfg(feature = "publication")]

#[path = "checkpoint/external_delta.rs"]
mod external_delta;

#[path = "checkpoint/frontier_admission.rs"]
mod frontier_admission;

#[path = "checkpoint/native_cache.rs"]
mod native_cache;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use crab_read::capsule_protocol::{CapsuleReadLimits, CapsuleRepositoryView};
use crab_storage::{
    ImmutableWriteVerification, StorageObservation, StorageObserver, StorageOperation, Store,
    StoreLayout,
};
use tokio_util::sync::CancellationToken;

const LIMIT: u64 = 8 * 1024 * 1024;
const LIMITS: CapsuleReadLimits = CapsuleReadLimits {
    max_capsule_bytes: LIMIT,
    max_frontier_bytes: LIMIT,
};

#[derive(Default)]
struct Observations(
    Mutex<Vec<StorageObservation>>,
    Mutex<Option<CancellationToken>>,
);

impl StorageObserver for Observations {
    fn started(&self, _: StorageOperation) {}

    fn finished(&self, observation: StorageObservation) {
        self.0.lock().unwrap().push(observation);
        if observation.operation == StorageOperation::Range
            && let Some(cancel) = self.1.lock().unwrap().as_ref()
        {
            cancel.cancel();
        }
    }
}

async fn fixture() -> (StoreLayout<Store>, Arc<Observations>) {
    let (layout, observations) = empty_fixture().await;
    for name in ["first", "other"] {
        publish_blob(&layout, name).await;
    }
    (layout, observations)
}

#[tokio::test]
async fn maintenance_reuses_committed_checkpoint_without_reloading_authority() {
    let (layout, observations) = fixture().await;
    let before = view(&layout).await;
    observations.0.lock().unwrap().clear();
    let outcome = crab_remote::checkpoint::maintain_capsule_repository(
        &layout,
        1,
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let requests = observations.0.lock().unwrap().clone();
    assert!(outcome.published);
    let after = view(&layout).await;
    assert_eq!(after.refs(), before.refs());
    assert_eq!(
        after.git_visibility_index().unwrap().ref_closures(),
        before.git_visibility_index().unwrap().ref_closures()
    );
    assert_eq!(after.git_pack_count(), 1);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.operation == StorageOperation::Get)
            .count(),
        3,
        "read the initial root and two ref heads, not our committed root/checkpoint"
    );
}

#[tokio::test]
async fn retained_checkpoint_view_requires_complete_metadata_and_exact_root_binding() {
    let (layout, _) = fixture().await;
    let checkpointed = checkpoint(&layout).await;
    let layered = checkpointed.layered_checkpoint().unwrap();
    publish_blob(&layout, "later").await;
    let retained = crab_read::capsule_protocol::compacted_view_from_checkpoint(
        checkpointed.root_snapshot().clone(),
        layered.clone(),
        LIMITS,
    )
    .unwrap();
    let loaded = crab_read::capsule_protocol::open_compacted_view_from_root(
        &layout,
        checkpointed.root_snapshot().clone(),
        LIMITS,
    )
    .await
    .unwrap();
    assert_eq!(retained.refs(), loaded.refs());
    assert_eq!(retained.layered_checkpoint(), loaded.layered_checkpoint());
    assert!(retained.visible_ref_transactions().is_empty());
    assert!(!retained.refs().contains_key("refs/tags/later"));
    verify_git_blobs(
        &layout,
        &retained,
        &[("first", b"first"), ("other", b"other")],
    )
    .await;

    let footer = crab_metadata::capsule_protocol::LayeredCheckpoint::decode_control(
        layered.bytes().slice(layered.control_offset() as usize..),
        layered.bytes().len() as u64,
        layered.control_offset(),
        layered.hash(),
        layered.footer_hash(),
    )
    .unwrap();
    assert!(
        crab_read::capsule_protocol::compacted_view_from_checkpoint(
            checkpointed.root_snapshot().clone(),
            footer,
            LIMITS,
        )
        .is_err()
    );
    assert!(matches!(
        crab_read::capsule_protocol::compacted_view_from_checkpoint(
            checkpointed.root_snapshot().clone(),
            layered.clone(),
            CapsuleReadLimits {
                max_capsule_bytes: layered.bytes().len() as u64 - 1,
                ..LIMITS
            },
        ),
        Err(crab_read::ReadError::CapsuleReadLimit { .. })
    ));
    let newer = checkpoint(&layout).await;
    assert!(
        crab_read::capsule_protocol::compacted_view_from_checkpoint(
            newer.root_snapshot().clone(),
            layered.clone(),
            LIMITS,
        )
        .is_err()
    );
}

#[tokio::test]
async fn pinned_maintenance_preserves_later_heads_and_reports_its_own_inventory() {
    let (layout, observations) = fixture().await;
    let before = view(&layout).await;
    let later = publish_blob(&layout, "later").await;
    observations.0.lock().unwrap().clear();
    let outcome = crab_remote::checkpoint::maintain_capsule_repository_from_view(
        &layout,
        &before,
        1,
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let requests = observations.0.lock().unwrap().clone();
    assert!(outcome.checkpointed.published);
    assert!(outcome.repacked.published);
    assert_eq!(outcome.packs_after, 1);
    assert_eq!(outcome.checkpointed.pack_bytes_read, 0);
    assert_eq!(
        outcome.repacked.pack_bytes_read,
        before.git_pack_bytes().unwrap()
    );
    assert!(requests.iter().all(|request| !matches!(
        request.operation,
        StorageOperation::Get | StorageOperation::List
    )));
    let after = view(&layout).await;
    assert_eq!(after.git_pack_count(), 2);
    assert_eq!(after.refs().get("refs/tags/later"), Some(&later));
    assert_eq!(after.root().root().refs(), before.refs());
    assert_eq!(
        after.root().root().compacted_ref_transactions(),
        before.visible_ref_transactions()
    );
    assert_eq!(
        outcome.bytes_after,
        after.layered_checkpoint().unwrap().sources()[0]
            .compressed_bytes()
            .unwrap()
    );
    verify_git_blobs(
        &layout,
        &after,
        &[
            ("first", b"first"),
            ("other", b"other"),
            ("later", b"later"),
        ],
    )
    .await;
}

#[tokio::test]
async fn pinned_maintenance_stops_after_losing_logical_publication() {
    let (layout, observations) = fixture().await;
    checkpoint(&layout).await;
    publish_blob(&layout, "later").await;
    let before = view(&layout).await;
    let winner = crab_write::capsule_protocol::retarget_head(
        &layout,
        before.root_snapshot().clone(),
        "refs/heads/main",
        "refs/heads/changed",
    )
    .await
    .unwrap();
    observations.0.lock().unwrap().clear();
    let outcome = crab_remote::checkpoint::maintain_capsule_repository_from_view(
        &layout,
        &before,
        1,
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!outcome.checkpointed.published);
    assert_eq!(
        outcome.repacked,
        crab_remote::checkpoint::CheckpointOutcome::default()
    );
    assert!(
        observations
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.operation != StorageOperation::Range)
    );
    assert_eq!(outcome.packs_after, before.git_pack_count());
    assert_eq!(outcome.bytes_after, before.git_pack_bytes().unwrap());
    let after = view(&layout).await;
    assert_eq!(after.root().digest(), winner.record().digest());
    assert_eq!(after.refs(), before.refs());
    verify_git_blobs(
        &layout,
        &after,
        &[
            ("first", b"first"),
            ("other", b"other"),
            ("later", b"later"),
        ],
    )
    .await;
}

#[tokio::test]
async fn maintenance_repacks_checkpoint_debt_without_folding_a_below_threshold_frontier() {
    let (layout, _) = fixture().await;
    let checkpointed = checkpoint(&layout).await;
    publish_blob(&layout, "later").await;
    let before = view(&layout).await;
    let outcome = crab_remote::checkpoint::maintain_capsule_repository_from_view(
        &layout,
        &before,
        32,
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.checkpointed,
        crab_remote::checkpoint::CheckpointOutcome::default()
    );
    assert!(outcome.repacked.published);
    assert_eq!(
        outcome.repacked.pack_bytes_read,
        checkpointed.git_pack_bytes().unwrap()
    );
    let after = view(&layout).await;
    assert_eq!(
        after.root().root().refs(),
        checkpointed.root().root().refs()
    );
    assert_eq!(
        after.root().root().compacted_ref_transactions(),
        checkpointed.root().root().compacted_ref_transactions()
    );
    assert_eq!(
        after.root().root().history(),
        checkpointed.root().root().history()
    );
    assert_eq!(after.refs(), before.refs());
    assert_eq!(after.git_pack_count(), 2);
    verify_git_blobs(
        &layout,
        &after,
        &[
            ("first", b"first"),
            ("other", b"other"),
            ("later", b"later"),
        ],
    )
    .await;
}

#[tokio::test]
async fn cancelled_physical_maintenance_keeps_its_completed_logical_publication() {
    let (layout, observations) = fixture().await;
    let before = view(&layout).await;
    let cancel = CancellationToken::new();
    *observations.1.lock().unwrap() = Some(cancel.clone());
    let outcome = crab_remote::checkpoint::maintain_capsule_repository_from_view(
        &layout, &before, 1, LIMIT, &cancel,
    )
    .await;
    assert!(matches!(
        outcome,
        Err(crab_remote::checkpoint::CheckpointError::Cancelled)
    ));
    *observations.1.lock().unwrap() = None;
    let after = view(&layout).await;
    assert_eq!(after.refs(), before.refs());
    assert_eq!(after.layered_checkpoint().unwrap().sources().len(), 2);
    assert_eq!(
        after.root().root().compacted_ref_transactions(),
        before.visible_ref_transactions()
    );
    assert!(after.root().root().history().is_some());
    verify_git_blobs(&layout, &after, &[("first", b"first"), ("other", b"other")]).await;
}

#[tokio::test]
async fn compacted_frontier_uses_resident_indexes_and_verifies_payloads() {
    use object_store::ObjectStoreExt;
    let (layout, observations) = empty_fixture().await;
    publish_blob(&layout, "seed").await;
    checkpoint(&layout).await;
    let mut old_oid = None;
    let mut expected = Vec::new();
    for ordinal in 0..32_u32 {
        let body = (0..4096_u32)
            .flat_map(|chunk| {
                *blake3::hash(&[ordinal.to_le_bytes(), chunk.to_le_bytes()].concat()).as_bytes()
            })
            .collect::<Vec<_>>();
        let (oid, pack) = blob_pack(&body);
        assert!(pack.pack_bytes().len() > 64 * 1024);
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        let name = "refs/tags/frontier";
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                name,
                old_oid.clone(),
                Some(oid.clone()),
                None,
            )],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name.to_owned(),
            GitVisibilityEdit::from_replacement_objects(
                old_oid.clone(),
                oid.clone(),
                vec![oid.clone()],
            ),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![pack],
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
            .await
            .unwrap();
        old_oid = Some(oid.clone());
        expected.push((gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap(), body));
    }
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    // A lazy source exercises the bounded origin-read and corruption contract;
    // small resident frontiers correctly need no post-open storage request.
    let view = crab_read::capsule_protocol::open_view_from_root_with_control(&layout, root, LIMITS)
        .await
        .unwrap();
    assert_eq!(view.capsule_run_sources().len(), 1);
    let source = &view.capsule_run_sources()[0];
    let oids = expected.iter().map(|(oid, _)| *oid).collect::<Vec<_>>();
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let cancel = CancellationToken::new();
    let repository = view
        .git_repository_from_store(
            layout.clone(),
            crab_remote_git::RepositoryIdentity::new("fixture", "pooled-indexes", 1).unwrap(),
            runtime.clone(),
            crab_remote_git::RepositoryOptions::default(),
            LIMIT,
            &cancel,
        )
        .await
        .unwrap();
    let operation = repository
        .operation(crab_remote_git::OperationKind::UploadPack, &cancel)
        .await
        .unwrap();
    observations.0.lock().unwrap().clear();
    let result = operation.pinned_object_metadata(&oids).await;
    operation.finish(result).await.unwrap();
    let reads = observations.0.lock().unwrap().clone();
    assert!(
        reads.is_empty(),
        "authenticated resident indexes need no origin read"
    );
    let operation = repository
        .operation(crab_remote_git::OperationKind::UploadPack, &cancel)
        .await
        .unwrap();
    let result = operation.read_objects(&oids).await;
    let objects = operation.finish(result).await.unwrap();
    assert_eq!(objects.len(), expected.len());
    for (object, (oid, bytes)) in objects.iter().zip(&expected) {
        assert_eq!(object.oid, *oid);
        assert_eq!(object.data.as_ref(), bytes);
    }
    runtime.shutdown().await;

    let path = layout.capsule_path(source.object_hash());
    let (original, _) = layout.store().get_with_etag(&path).await.unwrap();
    for scenario in ["byte-budget", "corrupt-index"] {
        if scenario == "corrupt-index" {
            let mut corrupt = original.to_vec();
            corrupt[source.members().last().unwrap().index().offset() as usize] ^= 1;
            layout
                .store()
                .inner()
                .put(&path, Bytes::from(corrupt).into())
                .await
                .unwrap();
        }
        let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
        let repository = view
            .git_repository_from_store(
                layout.clone(),
                crab_remote_git::RepositoryIdentity::new("fixture", "pooled-indexes", 1).unwrap(),
                runtime.clone(),
                crab_remote_git::RepositoryOptions::default(),
                LIMIT,
                &cancel,
            )
            .await;
        if scenario == "corrupt-index" {
            assert!(repository.is_err(), "corrupt pooled index must fail intake");
            assert_eq!(runtime.snapshot().await.pack_index_entries, 0);
            runtime.shutdown().await;
            continue;
        }
        let repository = repository.unwrap();
        let operation = repository
            .operation_with_limits(
                crab_remote_git::OperationKind::UploadPack,
                &cancel,
                crab_remote_git::OperationLimits {
                    max_storage_requests: 1,
                    max_fetched_bytes: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let result = operation.read_objects(&oids[..1]).await;
        let result = operation.finish(result).await;
        let mut error = result.as_ref().unwrap_err();
        while let crab_remote_git::Error::SharedRead { source } = error {
            error = source.as_ref();
        }
        assert!(matches!(
            error,
            crab_remote_git::Error::LimitExceeded {
                limit: "fetched bytes",
                ..
            }
        ));
        assert_eq!(runtime.snapshot().await.pack_index_entries, 0, "{scenario}");
        runtime.shutdown().await;
    }
}

async fn empty_fixture() -> (StoreLayout<Store>, Arc<Observations>) {
    let observations = Arc::new(Observations::default());
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_immutable_write_verification(ImmutableWriteVerification::Sha256Checksum)
        .with_storage_observer(observations.clone());
    let layout = StoreLayout::new(store, "repositories/checkpoint".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    (layout, observations)
}

async fn publish_blob(layout: &StoreLayout<Store>, name: &str) -> String {
    publish_blobs(layout, &[(name, name.as_bytes())]).await[0].clone()
}

fn blob_pack(bytes: &[u8]) -> (String, CapsuleGitPack) {
    let oid = crab_remote::objects::object_id(gix_object::Kind::Blob, bytes).unwrap();
    let mut pack = Vec::new();
    crab_git::pack_writer::write_pack(
        &mut pack,
        std::iter::once(Ok((gix_object::Kind::Blob, bytes.len() as u64, bytes))),
        LIMIT,
        || false,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let pack_path = directory.path().join("source.pack");
    std::fs::write(&pack_path, &pack).unwrap();
    let installed = crab_git::pack::install_pack_file_from_path(
        &directory.path().join("indexed"),
        &pack_path,
        blake3::hash(&pack).to_hex().as_ref(),
        LIMIT,
        true,
    )
    .unwrap();
    let checksum = gix_hash::ObjectId::from_hex(installed.git_sha1.as_bytes()).unwrap();
    let kinds =
        crab_git::pack_locator::encode_pack_kind_metadata(checksum, &[gix_object::Kind::Blob])
            .unwrap();
    let pack = CapsuleGitPack::new(
        Bytes::from(pack),
        Bytes::from(std::fs::read(installed.idx_path).unwrap()),
        Bytes::from(std::fs::read(installed.rev_path).unwrap()),
        Bytes::from(kinds),
        installed.git_sha1,
        1,
    )
    .unwrap();
    (oid.to_string(), pack)
}

async fn publish_blobs(layout: &StoreLayout<Store>, blobs: &[(&str, &[u8])]) -> Vec<String> {
    let root = crab_write::capsule_protocol::open_root(layout)
        .await
        .unwrap();
    let mut packs = Vec::new();
    let mut edits = Vec::new();
    let mut visibility = BTreeMap::new();
    let mut oids = Vec::new();
    for (name, bytes) in blobs {
        let (oid, pack) = blob_pack(bytes);
        let ref_name = format!("refs/tags/{name}");
        edits.push(CapsuleRefEdit::new(
            &ref_name,
            None,
            Some(oid.clone()),
            None,
        ));
        visibility.insert(
            ref_name,
            GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid.clone()]),
        );
        packs.push(pack);
        oids.push(oid);
    }
    let transaction = CapsuleTransaction::new(root.record().digest(), edits).unwrap();
    let visibility = CapsuleVisibilityDelta::new(visibility).unwrap();
    let capsule = Capsule::build(
        &transaction,
        packs,
        vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            visibility.encode().unwrap(),
        )],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(layout, root, &transaction, &capsule)
        .await
        .unwrap();
    oids
}

async fn view(layout: &StoreLayout<Store>) -> CapsuleRepositoryView {
    let root = crab_write::capsule_protocol::open_root(layout)
        .await
        .unwrap();
    crab_read::capsule_protocol::open_view_from_root_with_control(layout, root, LIMITS)
        .await
        .unwrap()
}

async fn checkpoint(layout: &StoreLayout<Store>) -> CapsuleRepositoryView {
    let before = view(layout).await;
    assert!(
        crab_remote::checkpoint::publish_capsule_checkpoint_from_view(
            layout,
            &before,
            0,
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .published
    );
    view(layout).await
}

async fn verify_git_blobs(
    layout: &StoreLayout<Store>,
    view: &CapsuleRepositoryView,
    blobs: &[(&str, &[u8])],
) -> bool {
    let directory = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(directory.path()).unwrap();
    let installed = {
        let view = view.clone();
        let layout = layout.clone();
        let git_dir = directory.path().to_owned();
        tokio::spawn(async move {
            crab_read::capsule_protocol::install_git_packs_from_store(
                &view,
                &layout,
                &git_dir,
                LIMIT,
                None,
                &CancellationToken::new(),
            )
            .await
        })
        .await
        .unwrap()
        .unwrap()
    };
    verify_installed_git_blobs(view, directory.path(), &installed, blobs);
    installed.complete_visibility
}

fn verify_installed_git_blobs(
    view: &CapsuleRepositoryView,
    git_dir: &std::path::Path,
    installed: &crab_read::capsule_protocol::InstalledGitPacks,
    blobs: &[(&str, &[u8])],
) {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    if installed.complete_visibility {
        let checkpoint = view.layered_checkpoint().unwrap();
        let members = checkpoint
            .sources()
            .iter()
            .flat_map(|source| source.members())
            .map(|member| (format!("pack-{}", member.pack().blake3()), member))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(installed.paths.len(), members.len());
        for path in &installed.paths {
            let member = members[path.file_stem().unwrap().to_str().unwrap()];
            for (extension, expected) in [
                ("pack", member.pack()),
                ("idx", member.index()),
                ("rev", member.reverse_index()),
            ] {
                let bytes = std::fs::read(path.with_extension(extension)).unwrap();
                assert_eq!(blake3::hash(&bytes).to_hex().as_str(), expected.blake3());
            }
        }
    }
    for (name, oid) in view.refs() {
        git(&["update-ref", name, oid]);
    }
    for (name, bytes) in blobs {
        assert_eq!(
            git(&["cat-file", "blob", &format!("refs/tags/{name}")]),
            *bytes
        );
    }
    git(&["fsck", "--strict", "--full"]);
}

#[tokio::test]
async fn cold_clone_admission_does_not_omit_a_newer_ref_frontier() {
    let (layout, _) = fixture().await;
    checkpoint(&layout).await;
    publish_blob(&layout, "newer").await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let view = crab_read::capsule_protocol::open_view_from_root_with_layered_control(
        &layout, root, LIMITS,
    )
    .await
    .unwrap();

    assert!(
        view.layered_cold_clone_packs(&layout, LIMIT)
            .unwrap()
            .is_none()
    );
    verify_git_blobs(
        &layout,
        &view,
        &[
            ("first", b"first"),
            ("other", b"other"),
            ("newer", b"newer"),
        ],
    )
    .await;
}

#[tokio::test]
async fn cold_clone_admits_body_and_sidecars_against_one_budget_before_io() {
    let (layout, observations) = fixture().await;
    let checkpointed = checkpoint(&layout).await;
    let view = crab_read::capsule_protocol::open_view_from_root_with_layered_control(
        &layout,
        checkpointed.root_snapshot().clone(),
        LIMITS,
    )
    .await
    .unwrap();
    let body_bytes = view
        .layered_cold_clone_packs(&layout, LIMIT)
        .unwrap()
        .unwrap()
        .iter()
        .map(|pack| pack.pack_range.end - pack.pack_range.start)
        .sum::<u64>();
    observations.0.lock().unwrap().clear();
    let directory = tempfile::tempdir().unwrap();
    let error = crab_read::capsule_protocol::install_git_packs_from_store(
        &view,
        &layout,
        directory.path(),
        body_bytes,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        crab_read::ReadError::CapsuleReadLimit { .. }
    ));
    assert!(observations.0.lock().unwrap().is_empty());
    assert!(!directory.path().join("objects/pack").exists());
    assert!(verify_git_blobs(&layout, &view, &[("first", b"first"), ("other", b"other")]).await);
}

#[tokio::test]
async fn cold_clone_reuses_all_pack_indexes_with_overlapping_object_sets() {
    let (layout, _) = fixture().await;
    publish_blobs(&layout, &[("duplicate", b"first")]).await;
    let checkpointed = checkpoint(&layout).await;
    let view = crab_read::capsule_protocol::open_view_from_root_with_layered_control(
        &layout,
        checkpointed.root_snapshot().clone(),
        LIMITS,
    )
    .await
    .unwrap();
    assert!(
        verify_git_blobs(
            &layout,
            &view,
            &[
                ("first", b"first"),
                ("other", b"other"),
                ("duplicate", b"first")
            ]
        )
        .await
    );
}

#[tokio::test]
async fn cold_clone_rejects_corrupt_late_source_before_installing_any_pack() {
    let (layout, _) = fixture().await;
    let checkpointed = checkpoint(&layout).await;
    let view = crab_read::capsule_protocol::open_view_from_root_with_layered_control(
        &layout,
        checkpointed.root_snapshot().clone(),
        LIMITS,
    )
    .await
    .unwrap();
    let candidates = view
        .layered_cold_clone_packs(&layout, LIMIT)
        .unwrap()
        .unwrap();
    assert!(candidates.len() > 1);
    let last = candidates.last().unwrap();
    let (body, etag) = layout
        .store()
        .get_with_etag(&last.source_path)
        .await
        .unwrap();
    let mut corrupt = body.to_vec();
    corrupt[last.pack_range.start as usize] ^= 1;
    layout
        .store()
        .update(&last.source_path, Bytes::from(corrupt), etag)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let error = crab_read::capsule_protocol::install_git_packs_from_store(
        &view,
        &layout,
        directory.path(),
        LIMIT,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("pack content hash"), "{error}");
    assert!(
        std::fs::read_dir(directory.path().join("objects/pack"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn logical_checkpoint_does_not_read_or_replace_pack_sources() {
    let (layout, observations) = fixture().await;
    let before = view(&layout).await;
    let expected_sources = before.capsule_run_sources().to_vec();
    observations.0.lock().unwrap().clear();
    let outcome = crab_remote::checkpoint::publish_capsule_checkpoint_from_view(
        &layout,
        &before,
        0,
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(outcome.published);
    assert_eq!(
        (outcome.pack_bytes_read, outcome.pack_bytes_written),
        (0, 0)
    );
    let reads = observations
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|observation| {
            matches!(
                observation.operation,
                StorageOperation::Get | StorageOperation::Range
            )
        })
        .count();
    assert_eq!(
        reads, 0,
        "logical publication must not download its pack suffix"
    );
    let after = view(&layout).await;
    assert_eq!(
        after.layered_checkpoint().unwrap().sources(),
        expected_sources
    );
    assert_eq!(after.refs(), before.refs());
    assert_eq!(
        after.git_visibility_index().unwrap().ref_closures(),
        before.git_visibility_index().unwrap().ref_closures()
    );
}

#[tokio::test]
async fn first_checkpoint_opens_its_frontier_from_authenticated_controls() {
    let (layout, _) = fixture().await;
    assert!(
        crab_remote::checkpoint::publish_capsule_checkpoint(
            &layout,
            0,
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .published
    );
    let after = view(&layout).await;
    assert_eq!(after.layered_checkpoint().unwrap().sources().len(), 2);
    assert_eq!(
        after.git_visibility_index().unwrap().ref_closures().len(),
        2
    );
}

#[tokio::test]
async fn corrupt_repack_source_cannot_replace_the_logical_checkpoint() {
    use object_store::ObjectStoreExt;
    let (layout, _) = fixture().await;
    let before = checkpoint(&layout).await;
    let source = &before.layered_checkpoint().unwrap().sources()[0];
    let path = layout.capsule_path(source.object_hash());
    let (bytes, _) = layout.store().get_with_etag(&path).await.unwrap();
    let mut corrupt = bytes.to_vec();
    corrupt[source.members()[0].pack().offset() as usize] ^= 1;
    layout
        .store()
        .inner()
        .put(&path, Bytes::from(corrupt).into())
        .await
        .unwrap();
    assert!(
        crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
            &layout,
            before.root_snapshot().clone(),
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .is_err()
    );
    let current = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(current.record().digest(), before.root().digest());
}

#[tokio::test]
async fn physical_repack_preserves_newer_ref_heads_and_checkpoint_history() {
    let (layout, observations) = fixture().await;
    let before = checkpoint(&layout).await;
    let late = publish_blob(&layout, "later").await;
    assert!(
        crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
            &layout,
            before.root_snapshot().clone(),
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .published
    );
    let after = view(&layout).await;
    assert_eq!(after.layered_checkpoint().unwrap().sources().len(), 1);
    assert_eq!(after.root().root().refs(), before.root().root().refs());
    assert_eq!(
        after.root().root().compacted_ref_transactions(),
        before.root().root().compacted_ref_transactions()
    );
    assert_eq!(
        after.root().root().history(),
        before.root().root().history()
    );
    assert_eq!(
        after.root().root().generation(),
        before.root().root().generation()
    );
    assert_eq!(after.refs().get("refs/tags/later"), Some(&late));

    verify_git_blobs(
        &layout,
        &after,
        &[
            ("first", b"first"),
            ("other", b"other"),
            ("later", b"later"),
        ],
    )
    .await;

    observations.0.lock().unwrap().clear();
    let noop = crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
        &layout,
        after.root_snapshot().clone(),
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(noop, crab_remote::checkpoint::CheckpointOutcome::default());
    let operations = observations.0.lock().unwrap();
    assert_eq!(
        operations.len(),
        1,
        "already-geometric maintenance reads only checkpoint metadata"
    );
    assert_eq!(
        operations[0].bytes_read,
        after.root().root().checkpoint().unwrap().size()
    );
}

#[tokio::test]
async fn stale_repack_cannot_replace_a_newer_root() {
    let (layout, _) = fixture().await;
    let before = checkpoint(&layout).await;
    let changed = crab_write::capsule_protocol::retarget_head(
        &layout,
        before.root_snapshot().clone(),
        "refs/heads/main",
        "refs/heads/changed",
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let outcome = crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
            &layout,
            before.root_snapshot().clone(),
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!outcome.published);
        assert_eq!(outcome.pack_bytes_read, before.git_pack_bytes().unwrap());
        let layers = layout
            .store()
            .list_prefix(&layout.repo_path("v2/pack-layers"))
            .await
            .unwrap();
        assert_eq!(layers.len(), 1);
        let (bytes, _) = layout
            .store()
            .get_with_etag(&layers[0].location)
            .await
            .unwrap();
        let layer = crab_metadata::capsule_protocol::PackLayer::decode(bytes).unwrap();
        assert_eq!(
            outcome.pack_bytes_written,
            layer
                .source_descriptor()
                .unwrap()
                .compressed_bytes()
                .unwrap()
        );
        let current = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        assert_eq!(current.record().digest(), changed.record().digest());
    }
}

#[tokio::test]
async fn cancelled_repack_leaves_the_logical_checkpoint_visible() {
    let (layout, observations) = fixture().await;
    let before = checkpoint(&layout).await;
    let cancel = CancellationToken::new();
    *observations.1.lock().unwrap() = Some(cancel.clone());
    let result = crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
        &layout,
        before.root_snapshot().clone(),
        LIMIT,
        &cancel,
    )
    .await;
    assert!(matches!(
        result,
        Err(crab_remote::checkpoint::CheckpointError::Cancelled)
    ));
    let current = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(current.record().digest(), before.root().digest());
}

#[tokio::test]
async fn first_checkpoint_rolls_up_only_the_suffix_required_by_the_source_limit() {
    let (layout, _) = fixture().await;
    for ordinal in 2..65 {
        publish_blob(&layout, &format!("blob-{ordinal:02}")).await;
    }
    let before = view(&layout).await;
    assert_eq!(before.capsule_run_sources().len(), 65);
    let outcome = crab_remote::checkpoint::publish_capsule_checkpoint_from_view(
        &layout,
        &before,
        0,
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(outcome.published);
    let after = view(&layout).await;
    let sources = after.layered_checkpoint().unwrap().sources();
    assert_eq!(sources.len(), 64);
    assert_eq!(&sources[..63], &before.capsule_run_sources()[..63]);
    assert_eq!(
        outcome.pack_bytes_read,
        before.capsule_run_sources()[63..]
            .iter()
            .map(|source| source.compressed_bytes().unwrap())
            .sum::<u64>()
    );
    assert_eq!(
        outcome.pack_bytes_written,
        sources[63].compressed_bytes().unwrap()
    );
    assert_eq!(after.refs(), before.refs());
    assert_eq!(
        after.git_visibility_index().unwrap().ref_closures(),
        before.git_visibility_index().unwrap().ref_closures()
    );
}

#[tokio::test]
async fn compacted_repeated_packs_preserve_member_positions_and_ref_state() {
    let (layout, _) = empty_fixture().await;
    let packs = [blob_pack(b"first"), blob_pack(b"second")];
    let name = "refs/tags/repeated";
    let mut previous = None;
    for sequence in 0..32 {
        let (oid, pack) = &packs[sequence % packs.len()];
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                name,
                previous.clone(),
                Some(oid.clone()),
                None,
            )],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name.to_owned(),
            GitVisibilityEdit::from_replacement_objects(previous, oid.clone(), vec![oid.clone()]),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![pack.clone()],
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
            .await
            .unwrap();
        previous = Some(oid.clone());
    }

    let before = view(&layout).await;
    assert_eq!(before.capsule_run_sources().len(), 1);
    let members = before.capsule_run_sources()[0].members();
    assert_eq!(members.len(), 32);
    assert_eq!(members[0].pack().blake3(), members[2].pack().blake3());
    assert_ne!(members[0].pack().offset(), members[2].pack().offset());
    let after = checkpoint(&layout).await;
    assert_eq!(
        after.layered_checkpoint().unwrap().sources(),
        before.capsule_run_sources()
    );
    assert_eq!(after.refs(), before.refs());
    assert_eq!(
        after.root().root().compacted_ref_transactions(),
        before.visible_ref_transactions()
    );
    crab_read::capsule_protocol::verify_layered_source(
        &layout,
        &after.layered_checkpoint().unwrap().sources()[0],
        Some(&before.capsule_run_pointers()[0]),
        LIMIT,
    )
    .await
    .unwrap();
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let cancel = CancellationToken::new();
    let remote = after
        .git_repository_from_store(
            layout.clone(),
            crab_remote_git::RepositoryIdentity::new("fixture", "repeated-packs", 1).unwrap(),
            runtime.clone(),
            crab_remote_git::RepositoryOptions::default(),
            LIMIT,
            &cancel,
        )
        .await
        .unwrap();
    let operation = remote
        .operation(crab_remote_git::OperationKind::UploadPack, &cancel)
        .await
        .unwrap();
    let oids = packs
        .iter()
        .map(|(oid, _)| gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap())
        .collect::<Vec<_>>();
    let result = operation.read_objects(&oids).await;
    let objects = operation.finish(result).await.unwrap();
    assert_eq!(objects[0].data.as_ref(), b"first");
    assert_eq!(objects[1].data.as_ref(), b"second");
    runtime.shutdown().await;
    verify_git_blobs(&layout, &after, &[("repeated", b"second")]).await;
}

#[tokio::test]
async fn duplicate_suffix_pack_does_not_change_stable_member_positions() {
    let (layout, _) = empty_fixture().await;
    publish_blobs(
        &layout,
        &[("a-first", b"shared"), ("a-second", b"retained")],
    )
    .await;
    publish_blobs(&layout, &[("b-duplicate", b"shared")]).await;
    publish_blobs(&layout, &[("c-new", b"new")]).await;
    let before = checkpoint(&layout).await;
    let prefix = before.layered_checkpoint().unwrap().sources()[0].clone();
    assert_eq!(prefix.members().len(), 2);
    assert!(
        crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
            &layout,
            before.root_snapshot().clone(),
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .published
    );
    let after = view(&layout).await;
    assert_eq!(after.layered_checkpoint().unwrap().sources()[0], prefix);
    assert_eq!(
        after.git_visibility_index().unwrap().ref_closures(),
        before.git_visibility_index().unwrap().ref_closures()
    );
    verify_git_blobs(
        &layout,
        &after,
        &[
            ("a-first", b"shared"),
            ("a-second", b"retained"),
            ("b-duplicate", b"shared"),
            ("c-new", b"new"),
        ],
    )
    .await;
}

#[tokio::test]
async fn duplicate_only_suffix_can_be_consolidated() {
    let (layout, _) = empty_fixture().await;
    for name in ["first", "second"] {
        publish_blobs(&layout, &[(name, b"same content")]).await;
    }
    let before = checkpoint(&layout).await;
    assert_eq!(before.layered_checkpoint().unwrap().sources().len(), 2);
    assert!(
        crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
            &layout,
            before.root_snapshot().clone(),
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .published
    );
    let after = view(&layout).await;
    assert_eq!(after.layered_checkpoint().unwrap().sources().len(), 1);
    assert_eq!(after.refs(), before.refs());
    assert_eq!(
        after.git_visibility_index().unwrap().ref_closures(),
        before.git_visibility_index().unwrap().ref_closures()
    );
    verify_git_blobs(
        &layout,
        &after,
        &[("first", b"same content"), ("second", b"same content")],
    )
    .await;
}

#[tokio::test]
async fn suffix_repack_does_not_read_an_identical_stable_pack() {
    use object_store::ObjectStoreExt;
    let (layout, _) = empty_fixture().await;
    publish_blobs(
        &layout,
        &[("a-first", b"shared"), ("a-second", b"retained")],
    )
    .await;
    publish_blobs(&layout, &[("b-duplicate", b"shared")]).await;
    publish_blobs(&layout, &[("c-new", b"new")]).await;
    let before = checkpoint(&layout).await;
    let prefix = &before.layered_checkpoint().unwrap().sources()[0];
    let path = layout.capsule_path(prefix.object_hash());
    let (original, _) = layout.store().get_with_etag(&path).await.unwrap();
    let mut corrupt = original.to_vec();
    corrupt[prefix.members()[0].pack().offset() as usize] ^= 1;
    layout
        .store()
        .inner()
        .put(&path, Bytes::from(corrupt).into())
        .await
        .unwrap();
    let result = crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
        &layout,
        before.root_snapshot().clone(),
        LIMIT,
        &CancellationToken::new(),
    )
    .await;
    layout
        .store()
        .inner()
        .put(&path, original.into())
        .await
        .unwrap();
    assert!(
        result.unwrap().published,
        "only selected suffix sources may be read"
    );
}

#[tokio::test]
async fn layered_inventory_includes_new_frontier_in_complete_and_control_views() {
    let (layout, _) = fixture().await;
    let checkpointed = checkpoint(&layout).await;
    let old_packs = checkpointed.git_pack_count();
    let old_bytes = checkpointed.git_pack_bytes().unwrap();
    let old_objects = checkpointed.git_object_count().unwrap();
    let old_visibility = checkpointed.git_visibility_index().unwrap();
    let (_, new_pack) = blob_pack(b"late");
    publish_blob(&layout, "late").await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let complete = crab_read::capsule_protocol::open_view_from_root(&layout, root, LIMITS)
        .await
        .unwrap();
    let control = view(&layout).await;

    for current in [&complete, &control] {
        assert_eq!(current.git_pack_count(), old_packs + 1);
        assert_eq!(
            current.git_pack_bytes().unwrap(),
            old_bytes + new_pack.pack_size()
        );
        assert_eq!(current.git_object_count().unwrap(), old_objects + 1);
        assert_ne!(
            current.git_visibility_index().unwrap().pack_index_hash,
            old_visibility.pack_index_hash
        );
    }
}

#[tokio::test]
async fn incremental_install_revalidates_local_packs_without_remote_reads() {
    let (layout, observations) = fixture().await;
    let current = checkpoint(&layout).await;
    let wanted = current
        .refs()
        .values()
        .map(|oid| gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap())
        .collect::<Vec<_>>();
    let directory = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("--git-dir")
            .arg(directory.path())
            .args(["init", "--bare", "--quiet"])
            .status()
            .unwrap()
            .success()
    );
    let first = crab_read::capsule_protocol::install_layered_git_packs_for_fetch(
        &current,
        &layout,
        directory.path(),
        LIMIT,
        &wanted,
        &[],
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.len(), 2);
    observations.0.lock().unwrap().clear();

    let repeated = crab_read::capsule_protocol::install_layered_git_packs_for_fetch(
        &current,
        &layout,
        directory.path(),
        LIMIT,
        &wanted,
        &[],
        &CancellationToken::new(),
    )
    .await
    .expect("a fully installed selection remains admissible")
    .unwrap();
    assert!(repeated.is_empty());
    assert!(observations.0.lock().unwrap().is_empty());
    for name in ["first", "other"] {
        let oid = &current.refs()[&format!("refs/tags/{name}")];
        let actual = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(directory.path())
            .args(["cat-file", "blob", oid])
            .output()
            .unwrap();
        assert!(actual.status.success());
        assert_eq!(actual.stdout, name.as_bytes());
    }

    // Existing files are not an integrity proof: even a same-size mutation
    // must fail before the retry can claim connectivity or read origin data.
    for extension in ["pack", "idx", "rev"] {
        let path = first[0].with_extension(extension);
        let original = std::fs::read(&path).unwrap();
        let mut corrupt = original.clone();
        corrupt[0] ^= 1;
        std::fs::write(&path, corrupt).unwrap();
        let result = crab_read::capsule_protocol::install_layered_git_packs_for_fetch(
            &current,
            &layout,
            directory.path(),
            LIMIT,
            &wanted,
            &[],
            &CancellationToken::new(),
        )
        .await;
        assert!(result.is_err(), "corrupt local {extension} was admitted");
        assert!(observations.0.lock().unwrap().is_empty());
        std::fs::write(path, original).unwrap();
    }
}

#[tokio::test]
async fn uncheckpointed_reader_retains_origin_placement_and_exact_blob_bytes() {
    let (layout, observations) = fixture().await;
    let captured = view(&layout).await;
    observations.0.lock().unwrap().clear();
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let cancel = CancellationToken::new();
    let repository = captured
        .git_repository_from_store(
            layout.clone(),
            crab_remote_git::RepositoryIdentity::new("memory", "test", 1).unwrap(),
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            LIMIT,
            &cancel,
        )
        .await
        .unwrap();
    assert!(repository.matches_store_layout(&layout));
    assert!(repository.matches_snapshot(&captured.git_snapshot().unwrap()));
    let operation = repository
        .operation(crab_remote_git::OperationKind::Repository, &cancel)
        .await
        .unwrap();
    for name in ["first", "other"] {
        let oid =
            gix_hash::ObjectId::from_hex(captured.refs()[&format!("refs/tags/{name}")].as_bytes())
                .unwrap();
        let objects = operation.read_objects(&[oid]).await.unwrap();
        assert_eq!(objects[0].data.as_ref(), name.as_bytes());
    }
    operation.finish(Ok(())).await.unwrap();
    assert!(observations.0.lock().unwrap().is_empty());
    runtime.shutdown().await;
}

#[tokio::test]
async fn uncheckpointed_reader_preserves_pack_byte_admission_before_origin_reads() {
    let (layout, observations) = fixture().await;
    let captured = view(&layout).await;
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let maximum = captured.git_pack_bytes().unwrap() - 1;
    observations.0.lock().unwrap().clear();
    let result = captured
        .git_repository_from_store(
            layout,
            crab_remote_git::RepositoryIdentity::new("memory", "test", 1).unwrap(),
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            maximum,
            &CancellationToken::new(),
        )
        .await;
    runtime.shutdown().await;
    assert!(matches!(
        result,
        Err(crab_read::ReadError::CapsuleReadLimit { maximum: actual, .. }) if actual == maximum
    ));
    assert!(observations.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn capsule_git_snapshot_invalidates_a_reader_when_only_ref_positions_change() {
    let (layout, _) = fixture().await;
    let before = view(&layout).await;
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let repository = before
        .git_repository_from_store(
            layout.clone(),
            crab_remote_git::RepositoryIdentity::new("memory", "test", 1).unwrap(),
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    publish_blob(&layout, "new-ref").await;
    let after = view(&layout).await;
    let captured = before.git_snapshot().unwrap();
    let current = after.git_snapshot().unwrap();
    assert_eq!(before.root().digest(), after.root().digest());
    assert_eq!(captured.manifest.generation, current.manifest.generation);
    assert_ne!(captured.manifest_etag, current.manifest_etag);
    assert_eq!(current.manifest_etag, after.state_digest());
    assert!(repository.matches_snapshot(&captured));
    assert!(!repository.matches_snapshot(&current));
    assert_eq!(current.manifest.refs, *after.refs());
    assert_eq!(current.journal.refs, *after.refs());
    runtime.shutdown().await;
}

#[tokio::test]
async fn browse_index_attachment_rejects_ref_only_staleness_without_io() {
    use crab_metadata::capsule_protocol::BrowseIndexes;
    let (layout, observations) = fixture().await;
    let before = view(&layout).await;
    let indexes =
        BrowseIndexes::new(before.state_digest(), "a".repeat(64), "b".repeat(64)).unwrap();
    observations.0.lock().unwrap().clear();
    let attached = before
        .clone()
        .with_browse_indexes(Some(indexes.clone()))
        .git_snapshot()
        .unwrap();
    assert_eq!(
        attached.manifest.path_state_hash.as_deref(),
        Some(indexes.path_state_hash())
    );
    assert!(observations.0.lock().unwrap().is_empty());
    publish_blob(&layout, "later").await;
    let after = view(&layout).await;
    assert_eq!(before.root().digest(), after.root().digest());
    observations.0.lock().unwrap().clear();
    let stale = after
        .with_browse_indexes(Some(indexes))
        .git_snapshot()
        .unwrap();
    assert!(stale.manifest.commit_graph_hash.is_none() && stale.manifest.path_state_hash.is_none());
    assert!(observations.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn browse_indexes_are_optional_repairable_and_support_non_commit_tags() {
    use crab_metadata::capsule_protocol::load_browse_indexes;
    let (layout, _) = fixture().await;
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let identity = crab_remote_git::RepositoryIdentity::new("memory", "browse", 1).unwrap();
    let cancel = CancellationToken::new();
    let build = || {
        crab_remote::browse_indexes::ensure(
            &layout,
            &identity,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            LIMIT,
            &cancel,
        )
    };
    assert!(load_browse_indexes(&layout).await.unwrap().is_none());
    build().await.unwrap();
    let captured = view(&layout).await;
    let record = load_browse_indexes(&layout).await.unwrap().unwrap();
    assert_eq!(record.state_digest(), captured.state_digest());
    let path = layout.capsule_browse_indexes_path();
    let original = layout
        .store()
        .get_with_etag_bounded(&path, 4096)
        .await
        .unwrap();
    build().await.unwrap();
    assert_eq!(
        layout
            .store()
            .get_with_etag_bounded(&path, 4096)
            .await
            .unwrap(),
        original
    );
    assert_eq!(
        crab_cache::path_class::classify_path(path.as_ref()),
        crab_cache::path_class::PathClass::Mutable
    );
    for bytes in [
        Bytes::from_static(b"invalid"),
        Bytes::from(vec![b'!'; 8192]),
    ] {
        layout.store().put_overwrite(&path, bytes).await.unwrap();
        assert!(load_browse_indexes(&layout).await.is_err());
        build().await.unwrap();
        assert_eq!(
            load_browse_indexes(&layout).await.unwrap(),
            Some(record.clone())
        );
    }
    let snapshot = captured
        .with_browse_indexes(Some(record))
        .git_snapshot()
        .unwrap();
    assert_eq!(snapshot.manifest.refs.len(), 2);
    assert!(matches!(
        layout.store().head(&layout.manifest_path()).await,
        Err(crab_storage::StorageError::NotFound { .. })
    ));
    runtime.shutdown().await;
}

#[tokio::test]
async fn capsule_git_snapshot_deduplicates_shared_packs_across_full_and_control_views() {
    let (layout, _) = empty_fixture().await;
    publish_blobs(&layout, &[("one", b"same bytes")]).await;
    publish_blobs(&layout, &[("two", b"same bytes")]).await;
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    for checkpointed in [false, true] {
        if checkpointed {
            checkpoint(&layout).await;
        }
        let full = crab_read::capsule_protocol::open_view(&layout, LIMITS)
            .await
            .unwrap();
        let control = view(&layout).await;
        let expected = full.git_snapshot().unwrap();
        for captured in [full, control] {
            let snapshot = captured.git_snapshot().unwrap();
            assert_eq!(snapshot.manifest_etag, expected.manifest_etag);
            assert_eq!(
                snapshot.manifest.pack_index_hash,
                expected.manifest.pack_index_hash
            );
            assert_eq!(
                snapshot.manifest.git_validation_digest,
                expected.manifest.git_validation_digest
            );
            assert_eq!(snapshot.journal.packs.len(), 1);
            let repository = captured
                .git_repository_from_store(
                    layout.clone(),
                    crab_remote_git::RepositoryIdentity::new("memory", "test", 1).unwrap(),
                    Arc::clone(&runtime),
                    crab_remote_git::RepositoryOptions::default(),
                    LIMIT,
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(repository.matches_store_layout(&layout));
            assert!(repository.matches_snapshot(&snapshot));
            assert!(
                repository
                    .matches_pack_inventory(&snapshot.journal.packs)
                    .unwrap()
            );
            let operation = repository
                .operation(
                    crab_remote_git::OperationKind::Repository,
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            let oid =
                gix_hash::ObjectId::from_hex(snapshot.manifest.refs["refs/tags/two"].as_bytes())
                    .unwrap();
            let objects = operation.read_objects(&[oid]).await.unwrap();
            assert_eq!(objects[0].data.as_ref(), b"same bytes");
            operation.finish(Ok(())).await.unwrap();
        }
    }
    assert!(matches!(
        layout.store().head(&layout.manifest_path()).await,
        Err(crab_storage::StorageError::NotFound { .. })
    ));
    runtime.shutdown().await;
}

#[tokio::test]
async fn incremental_install_enforces_byte_budget_before_pack_payload_reads() {
    let (layout, observations) = empty_fixture().await;
    let large = (0..4096_u32)
        .flat_map(|ordinal| *blake3::hash(&ordinal.to_le_bytes()).as_bytes())
        .collect::<Vec<_>>();
    publish_blobs(&layout, &[("large", &large)]).await;
    publish_blob(&layout, "small").await;
    let before = checkpoint(&layout).await;
    let wanted = before
        .refs()
        .values()
        .map(|oid| gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap())
        .collect::<Vec<_>>();
    let sidecar_bytes = before
        .layered_checkpoint()
        .unwrap()
        .sources()
        .iter()
        .flat_map(|source| source.members())
        .map(|member| {
            member.locator().offset() + member.locator().length() - member.index().offset()
        })
        .sum::<u64>();
    let limit = 64 * 1024;
    assert!(sidecar_bytes < limit);
    assert!(before.git_pack_bytes().unwrap() > limit);
    let directory = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("--git-dir")
            .arg(directory.path())
            .args(["init", "--bare", "--quiet"])
            .status()
            .unwrap()
            .success()
    );
    observations.0.lock().unwrap().clear();
    let result = crab_read::capsule_protocol::install_layered_git_packs_for_fetch(
        &before,
        &layout,
        directory.path(),
        limit,
        &wanted,
        &[],
        &CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(result, Err(crab_read::ReadError::CapsuleReadLimit { maximum, .. }) if maximum == limit)
    );
    assert_eq!(
        observations
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|read| read.bytes_read)
            .sum::<u64>(),
        sidecar_bytes
    );
    assert!(
        std::fs::read_dir(directory.path().join("objects/pack"))
            .unwrap()
            .next()
            .is_none()
    );
    let installed = crab_read::capsule_protocol::install_layered_git_packs_for_fetch(
        &before,
        &layout,
        directory.path(),
        LIMIT,
        &wanted,
        &[],
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(installed.len(), 2);
    for oid in wanted {
        let actual = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(directory.path())
            .args(["cat-file", "blob", &oid.to_string()])
            .output()
            .unwrap();
        assert!(actual.status.success());
        let expected = if oid.to_string() == before.refs()["refs/tags/large"] {
            large.as_slice()
        } else {
            b"small"
        };
        assert_eq!(actual.stdout, expected);
    }
}
