#![expect(clippy::unwrap_used, reason = "test fixture assertions")]

use super::*;
use crab_metadata::capsule_protocol::{
    CapsuleGitPack, CapsuleRefEdit, CapsuleTransaction, RepositoryRoot, create_root,
};
use gix_object::Kind;
use std::io::Cursor;

const BLOBS: [&[u8]; 2] = [b"first intake member", b"second intake member"];

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
