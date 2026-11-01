#![expect(clippy::unwrap_used, reason = "test fixture assertions")]

use super::*;
use crab_metadata::capsule_protocol::{
    CapsuleGitPack, CapsuleRefEdit, CapsuleTransaction, RepositoryRoot, create_root,
};
use gix_object::Kind;
use std::io::Cursor;

const BLOBS: [&[u8]; 2] = [b"first intake member", b"second intake member"];

#[tokio::test(flavor = "multi_thread")]
async fn large_ref_frontiers_bound_active_reads_without_dropping_refs() {
    use crab_metadata::capsule_protocol::{
        CapsuleRefHead, CapsuleVisibilityDelta, capsule_ref_name_key,
    };
    use crab_metadata::git_visibility::GitVisibilityEdit;
    use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn send<T: Send>(future: T) -> T {
        future
    }

    #[derive(Default)]
    struct Reads {
        active: AtomicUsize,
        peak: AtomicUsize,
    }
    impl StorageObserver for Reads {
        fn started(&self, operation: StorageOperation) {
            if matches!(operation, StorageOperation::Get | StorageOperation::Range) {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
            }
        }
        fn finished(&self, observation: StorageObservation) {
            if matches!(
                observation.operation,
                StorageOperation::Get | StorageOperation::Range
            ) {
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    let reads = Arc::new(Reads::default());
    let backend = ThrottledStore::new(
        object_store::memory::InMemory::new(),
        ThrottleConfig {
            wait_get_per_call: std::time::Duration::from_millis(5),
            ..Default::default()
        },
    );
    let store = Store::new(Arc::new(backend)).with_storage_observer(reads.clone());
    let layout = StoreLayout::new(store, "intake/many-refs".into());
    let initial =
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap();
    create_root(&layout, initial.clone()).await.unwrap();
    let mut expected = BTreeMap::new();
    for index in 0..96 {
        let name = format!("refs/heads/developer-{index:03}");
        let oid = format!("{:040x}", index + 1);
        let transaction = CapsuleTransaction::new(
            initial.digest(),
            vec![CapsuleRefEdit::new(&name, None, Some(oid.clone()), None)],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name.clone(),
            GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid.clone()]),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            Vec::new(),
            vec![crab_metadata::capsule_protocol::CapsuleSection::new(
                crab_metadata::capsule_protocol::CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule).unwrap();
        let pointer = CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
            run.level(),
            run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        layout
            .store()
            .put(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let head = CapsuleRefHead::from_root(&name, initial.root().ref_epoch().into(), None, None)
            .unwrap();
        let state = head
            .successor_state(
                &BTreeSet::new(),
                Some(oid.clone()),
                None,
                transaction.id().unwrap(),
                vec![pointer],
            )
            .unwrap();
        layout
            .store()
            .put(
                &layout.capsule_ref_head_path(&capsule_ref_name_key(&name)),
                head.commit(state).unwrap().encode().unwrap(),
            )
            .await
            .unwrap();
        expected.insert(name, oid);
    }
    let root = load_root(&layout).await.unwrap();
    let limits = CapsuleReadLimits {
        max_capsule_bytes: u64::MAX,
        max_frontier_bytes: u64::MAX,
    };
    for mode in ["complete", "metadata", "git", "fetch"] {
        reads.peak.store(0, Ordering::SeqCst);
        let view = match mode {
            "complete" => send(open_view_from_root(&layout, root.clone(), limits)).await,
            "metadata" => {
                send(open_view_from_root_with_metadata(
                    &layout,
                    root.clone(),
                    limits,
                ))
                .await
            }
            "git" => send(open_git_view_from_root(&layout, root.clone(), limits)).await,
            _ => {
                send(open_view_from_root_with_layered_control(
                    &layout,
                    root.clone(),
                    limits,
                ))
                .await
            }
        }
        .unwrap();
        assert_eq!(view.refs(), &expected, "{mode}");
        let peak = reads.peak.load(Ordering::SeqCst);
        assert!((2..=32).contains(&peak), "{mode}: {peak} concurrent reads");
        assert_eq!(reads.active.load(Ordering::SeqCst), 0, "{mode}");
        if mode == "git" {
            reads.peak.store(0, Ordering::SeqCst);
            let transitions = send(view.tip_bound_transitions()).await.unwrap();
            assert_eq!(transitions.len(), expected.len());
            let peak = reads.peak.load(Ordering::SeqCst);
            assert!(
                (2..=32).contains(&peak),
                "transitions: {peak} concurrent reads"
            );
            assert_eq!(reads.active.load(Ordering::SeqCst), 0);
        }
    }
}

pub(super) fn git_pack(blob: &[u8]) -> (CapsuleGitPack, ObjectId) {
    let blob_oid = gix_object::compute_hash(gix_hash::Kind::Sha1, Kind::Blob, blob).unwrap();
    let mut tree = b"100644 payload\0".to_vec();
    tree.extend_from_slice(blob_oid.as_bytes());
    let tree_oid = gix_object::compute_hash(gix_hash::Kind::Sha1, Kind::Tree, &tree).unwrap();
    let commit = format!("tree {tree_oid}\nauthor Fixture <test@example.invalid> 1700000000 +0000\ncommitter Fixture <test@example.invalid> 1700000000 +0000\n\nintake\n").into_bytes();
    let commit_oid = gix_object::compute_hash(gix_hash::Kind::Sha1, Kind::Commit, &commit).unwrap();
    let objects = [
        (Kind::Blob, blob.to_vec()),
        (Kind::Tree, tree),
        (Kind::Commit, commit),
    ];
    let objects = objects
        .into_iter()
        .map(|(kind, body)| Ok((kind, body.len() as u64, Cursor::new(body))));
    let mut body = Vec::new();
    crab_git::pack_writer::write_pack(&mut body, objects, u64::MAX, || false).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.pack");
    std::fs::write(&source, &body).unwrap();
    let hash = blake3::hash(&body).to_hex().to_string();
    let indexed =
        crab_git::pack::install_pack_file_from_path(temp.path(), &source, &hash, 0, true).unwrap();
    let locations = crab_git::pack_locator::PackLocationIter::open(
        &indexed.idx_path,
        &indexed.rev_path,
        body.len() as u64,
    )
    .unwrap();
    let checksum = locations.pack_checksum();
    let kinds = locations
        .map(|entry| {
            let oid = entry.unwrap().oid;
            if oid == blob_oid {
                Kind::Blob
            } else if oid == tree_oid {
                Kind::Tree
            } else {
                assert_eq!(oid, commit_oid);
                Kind::Commit
            }
        })
        .collect::<Vec<_>>();
    let locator = crab_git::pack_locator::encode_pack_kind_metadata(checksum, &kinds).unwrap();
    let pack = CapsuleGitPack::new(
        Bytes::from(body),
        Bytes::from(std::fs::read(indexed.idx_path).unwrap()),
        Bytes::from(std::fs::read(indexed.rev_path).unwrap()),
        Bytes::from(locator),
        indexed.git_sha1,
        3,
    )
    .unwrap();
    (pack, commit_oid)
}

async fn candidate_fixture() -> (StoreLayout<Store>, CapsuleRepositoryView, Capsule) {
    let router = StoreLayout::new(
        Store::new(Arc::new(object_store::memory::InMemory::new())),
        "intake/test".to_owned(),
    );
    let initial =
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap();
    create_root(&router, initial.clone()).await.unwrap();
    let view = open_view(
        &router,
        CapsuleReadLimits {
            max_capsule_bytes: u64::MAX,
            max_frontier_bytes: u64::MAX,
        },
    )
    .await
    .unwrap();
    let mut packs = Vec::new();
    let mut edits = Vec::new();
    for (index, blob) in BLOBS.into_iter().enumerate() {
        let (pack, oid) = git_pack(blob);
        packs.push(pack);
        edits.push(CapsuleRefEdit::new(
            format!("refs/heads/branch-{index}"),
            None,
            Some(oid.to_string()),
            None,
        ));
    }
    let transaction = CapsuleTransaction::new(initial.digest(), edits).unwrap();
    let candidate = Capsule::build(&transaction, packs, Vec::new()).unwrap();
    (router, view, candidate)
}

#[tokio::test]
async fn over_budget_candidate_batch_exposes_no_packs() {
    let (_, view, candidate) = candidate_fixture().await;
    let payloads = payloads_from_capsule(candidate.clone()).unwrap();
    let limit = payloads
        .iter()
        .map(|p| p.pack.as_ref().unwrap().len())
        .max()
        .unwrap();
    let destination = tempfile::tempdir().unwrap();
    crab_git::pack::initialize_bare_git_dir(destination.path()).unwrap();

    let error = install_git_packs_with_candidates(&view, &[candidate], destination.path(), limit)
        .await
        .unwrap_err();

    assert!(
        matches!(error, ReadError::CapsuleReadLimit { resource: "Git pack intake", maximum } if maximum == limit)
    );
    assert_eq!(
        std::fs::read_dir(destination.path().join("objects/pack"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn over_budget_maintenance_batch_exposes_no_packs_or_kinds() {
    let (router, _, candidate) = candidate_fixture().await;
    let payloads = payloads_from_capsule(candidate.clone()).unwrap();
    let limit = payloads
        .iter()
        .map(|p| p.pack.as_ref().unwrap().len())
        .max()
        .unwrap();
    let destination = tempfile::tempdir().unwrap();
    crab_git::pack::initialize_bare_git_dir(destination.path()).unwrap();

    let error = install_layered_git_packs_from_store_sources_selected(
        &[],
        &[],
        &[candidate],
        &router,
        destination.path(),
        limit,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(error, ReadError::CapsuleReadLimit { resource: "Git pack intake", maximum } if maximum == limit)
    );
    assert_eq!(
        std::fs::read_dir(destination.path().join("objects/pack"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn rejected_resident_batch_preserves_existing_pack_files() {
    let (_, view, candidate) = candidate_fixture().await;
    let mut payloads = payloads_from_capsule(candidate.clone()).unwrap();
    let limit = payloads
        .iter()
        .map(|p| p.pack.as_ref().unwrap().len())
        .max()
        .unwrap();
    let destination = tempfile::tempdir().unwrap();
    crab_git::pack::initialize_bare_git_dir(destination.path()).unwrap();
    install_git_pack_payloads(destination.path(), vec![payloads.remove(0)], 0, true)
        .await
        .unwrap();
    let snapshot = || {
        std::fs::read_dir(destination.path().join("objects/pack"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.file_name().unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let before = snapshot();

    let error = install_git_packs_with_candidates(&view, &[candidate], destination.path(), limit)
        .await
        .unwrap_err();

    assert!(matches!(error, ReadError::CapsuleReadLimit { .. }));
    assert_eq!(snapshot(), before);
}

#[tokio::test]
async fn resident_batch_accepts_exact_and_unlimited_budgets() {
    let (router, view, candidate) = candidate_fixture().await;
    let payloads = payloads_from_capsule(candidate.clone()).unwrap();
    let total = payloads
        .iter()
        .map(|p| p.pack.as_ref().unwrap().len())
        .sum::<u64>();
    for native in [true, false] {
        for limit in [total, 0] {
            let destination = tempfile::tempdir().unwrap();
            crab_git::pack::initialize_bare_git_dir(destination.path()).unwrap();
            for _ in 0..2 {
                let installed = if native {
                    install_git_packs_with_candidates(
                        &view,
                        std::slice::from_ref(&candidate),
                        destination.path(),
                        limit,
                    )
                    .await
                    .unwrap()
                } else {
                    install_layered_git_packs_from_store_sources_selected(
                        &[],
                        &[],
                        std::slice::from_ref(&candidate),
                        &router,
                        destination.path(),
                        limit,
                        None,
                        &CancellationToken::new(),
                    )
                    .await
                    .unwrap()
                };
                assert_eq!(installed.len(), 2);
            }
            for blob in BLOBS {
                let oid = gix_object::compute_hash(gix_hash::Kind::Sha1, Kind::Blob, blob).unwrap();
                let output = std::process::Command::new("git")
                    .arg("--git-dir")
                    .arg(destination.path())
                    .args(["cat-file", "blob", &oid.to_string()])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                assert_eq!(output.stdout, blob);
            }
        }
    }
}
