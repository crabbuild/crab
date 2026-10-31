use super::*;
use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use crab_storage::{
    ImmutableWriteVerification, StorageObservation, StorageObserver, StorageOperation,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Default)]
struct Observations(Mutex<Vec<StorageObservation>>);

impl StorageObserver for Observations {
    fn started(&self, _: StorageOperation) {}

    fn finished(&self, observation: StorageObservation) {
        self.0.lock().unwrap().push(observation);
    }
}

async fn fixture(
    count: usize,
) -> (
    crab_storage::StoreLayout<crab_storage::Store>,
    Vec<u64>,
    Arc<Observations>,
) {
    let bodies = (0..count).map(|ordinal| {
        if ordinal == 0 {
            (0..2048_u32)
                .flat_map(|index| *blake3::hash(&index.to_le_bytes()).as_bytes())
                .collect::<Vec<_>>()
        } else {
            format!("recent blob {ordinal}").into_bytes()
        }
    });
    fixture_with_bodies(bodies).await
}

async fn fixture_with_bodies(
    bodies: impl IntoIterator<Item = Vec<u8>>,
) -> (
    crab_storage::StoreLayout<crab_storage::Store>,
    Vec<u64>,
    Arc<Observations>,
) {
    let observations = Arc::new(Observations::default());
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_immutable_write_verification(ImmutableWriteVerification::Sha256Checksum)
        .with_storage_observer(observations.clone());
    let layout = crab_storage::StoreLayout::new(store, "org/capsule-repack-statistics".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let mut sizes = Vec::new();
    for (ordinal, body) in bodies.into_iter().enumerate() {
        let kind = gix_object::Kind::Blob;
        let oid = crab_remote::objects::object_id(kind, &body)
            .unwrap()
            .to_string();
        let mut bytes = Vec::new();
        crab_git::pack_writer::write_pack(
            &mut bytes,
            std::iter::once(Ok((kind, body.len() as u64, body.as_slice()))),
            LIMIT,
            || false,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.pack");
        std::fs::write(&source, &bytes).unwrap();
        let indexed = crab_git::pack::install_pack_file_from_path(
            &directory.path().join("indexed"),
            &source,
            blake3::hash(&bytes).to_hex().as_ref(),
            LIMIT,
            true,
        )
        .unwrap();
        let checksum = gix_hash::ObjectId::from_hex(indexed.git_sha1.as_bytes()).unwrap();
        let kinds = crab_git::pack_locator::encode_pack_kind_metadata(checksum, &[kind]).unwrap();
        sizes.push(bytes.len() as u64);
        let pack = CapsuleGitPack::new(
            Bytes::from(bytes),
            Bytes::from(std::fs::read(indexed.idx_path).unwrap()),
            Bytes::from(std::fs::read(indexed.rev_path).unwrap()),
            Bytes::from(kinds),
            indexed.git_sha1,
            1,
        )
        .unwrap();
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        let name = format!("refs/tags/blob-{ordinal}");
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(&name, None, Some(oid.clone()), None)],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name,
            GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid]),
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
        if ordinal == 0 {
            let root = crab_write::capsule_protocol::open_root(&layout)
                .await
                .unwrap();
            let view = open_repack_view(&layout, root, LIMIT).await.unwrap();
            assert!(
                crab_remote::checkpoint::publish_capsule_checkpoint_from_view(
                    &layout,
                    &view,
                    1,
                    LIMIT,
                    &CancellationToken::new(),
                )
                .await
                .unwrap()
                .published
            );
        }
    }
    (layout, sizes, observations)
}

#[tokio::test]
async fn layered_dry_run_reports_zero_pack_io_without_publishing() {
    let (layout, sizes, observations) = fixture(9).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let before = root.record().digest().to_owned();
    observations.0.lock().unwrap().clear();
    let outcome = run_repack_from_root(
        &Store::from_storage(layout.store().clone()),
        layout.repo_prefix(),
        root,
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.packs_before, 9);
    assert_eq!(outcome.packs_after, outcome.packs_before);
    assert_eq!(outcome.bytes_before, sizes.iter().sum::<u64>());
    assert_eq!(outcome.bytes_after, outcome.bytes_before);
    assert!(
        observations
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|item| item.bytes_written == 0)
    );
    let after = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(after.record().digest(), before);
    assert_eq!((outcome.bytes_read, outcome.bytes_written), (0, 0));
}

#[tokio::test]
async fn layered_repack_reports_only_selected_body_io_and_zero_for_a_following_noop() {
    let (layout, sizes, _) = fixture(3).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let store = Store::from_storage(layout.store().clone());
    let outcome = run_repack_from_root(
        &store,
        layout.repo_prefix(),
        root,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!((outcome.packs_before, outcome.packs_after), (3, 2));
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let after = open_repack_view(&layout, root.clone(), LIMIT)
        .await
        .unwrap();
    let sources = after.layered_checkpoint().unwrap().sources();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].compressed_bytes().unwrap(), sizes[0]);
    assert_eq!(outcome.bytes_read, sizes[1..].iter().sum::<u64>());
    assert_eq!(
        outcome.bytes_written,
        sources[1].compressed_bytes().unwrap()
    );
    let noop = run_repack_from_root(
        &store,
        layout.repo_prefix(),
        root,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!((noop.bytes_read, noop.bytes_written), (0, 0));
    assert_eq!(noop.bytes_after, outcome.bytes_after);
}

#[tokio::test]
async fn repack_preserves_gc_fence_failure_without_publishing() {
    let (layout, _, _) = fixture(3).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let fence_id = "e".repeat(64);
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let fenced = crab_write::capsule_protocol::begin_gc(
        &layout,
        root,
        crab_metadata::capsule_protocol::GcFence::new(&fence_id, expires_at).unwrap(),
    )
    .await
    .unwrap();
    let digest = fenced.record().digest().to_owned();
    let result = run_repack_from_root(
        &Store::from_storage(layout.store().clone()),
        layout.repo_prefix(),
        fenced,
        false,
        &CancellationToken::new(),
    )
    .await;
    let current = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let actual = current.record().digest().to_owned();
    let released = crab_write::capsule_protocol::end_gc(&layout, current, &fence_id)
        .await
        .unwrap();
    assert!(
        matches!(result, Err(CrabError::PushLockHeld { holder, expires_at_unix: Some(expiry), .. }) if holder == fence_id && expiry == expires_at)
    );
    assert_eq!(actual, digest);
    assert!(released.record().root().gc_fence().is_none());
}

#[tokio::test]
async fn apply_and_preview_views_keep_small_frontier_pack_bodies_cold() {
    let body = (0..8192_u32)
        .flat_map(|index| *blake3::hash(&index.to_le_bytes()).as_bytes())
        .collect::<Vec<_>>();
    let (layout, sizes, observations) =
        fixture_with_bodies([b"stable prefix".to_vec(), body]).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let before = root.record().digest().to_owned();
    observations.0.lock().unwrap().clear();
    // Apply retains complete visibility and member admission for consolidation,
    // while both captures leave pack bodies at their immutable origins.
    let apply_view = open_repack_view(&layout, root.clone(), LIMIT)
        .await
        .unwrap();
    assert_eq!(apply_view.git_pack_count(), 2);
    assert!(apply_view.capsules().is_empty());
    let visibility = apply_view.git_visibility_index().await.unwrap();
    assert_eq!(visibility.ref_count(), 2);
    for (name, tip) in apply_view.refs() {
        assert!(visibility.contains_hex_in_ref(name, tip));
    }
    assert_eq!(
        apply_view
            .capsule_run_member_oids()
            .values()
            .flat_map(|members| members.iter())
            .map(Vec::len)
            .sum::<usize>(),
        1
    );
    drop(apply_view);
    let apply_bytes = observations
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|item| item.bytes_read)
        .sum::<u64>();
    observations.0.lock().unwrap().clear();
    let outcome = run_repack_from_root(
        &Store::from_storage(layout.store().clone()),
        layout.repo_prefix(),
        root,
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let preview_bytes = observations
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|item| item.bytes_read)
        .sum::<u64>();
    println!(
        "preview body admission: apply_view_bytes={apply_bytes} preview_bytes={preview_bytes} frontier_pack_bytes={}",
        sizes[1]
    );
    assert!(apply_bytes < sizes[1]);
    assert!(preview_bytes < sizes[1]);
    assert_eq!((outcome.bytes_read, outcome.bytes_written), (0, 0));
    assert_eq!((outcome.packs_before, outcome.packs_after), (2, 2));
    let current = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(current.record().digest(), before);
}
