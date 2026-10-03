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
use std::sync::Mutex;

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
    let observations = Arc::new(Observations::default());
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_immutable_write_verification(ImmutableWriteVerification::Sha256Checksum)
        .with_storage_observer(observations.clone());
    let layout = crab_storage::StoreLayout::new(store, "org/capsule-repack-statistics".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let mut sizes = Vec::new();
    for ordinal in 0..count {
        let body = if ordinal == 0 {
            (0..2048_u32)
                .flat_map(|index| *blake3::hash(&index.to_le_bytes()).as_bytes())
                .collect::<Vec<_>>()
        } else {
            format!("recent blob {ordinal}").into_bytes()
        };
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
        &RepackConfig {
            dry_run: true,
            ..Default::default()
        },
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
        &RepackConfig::default(),
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
        &RepackConfig::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!((noop.bytes_read, noop.bytes_written), (0, 0));
    assert_eq!(noop.bytes_after, outcome.bytes_after);
}
