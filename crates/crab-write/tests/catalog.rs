use bytes::Bytes;
use crab_git::incoming_pack::{ReceiveLimits, quarantine};
use crab_metadata::{
    git_object_locator::{GitLocatorCoverage, GitObjectLocatorWriter},
    git_visibility::{self, GitVisibilityIndex},
    manifest_store,
    manifests::{BulkData, Manifest, PackManifestEntry, compact_pack_index},
};
use crab_remote_git::{
    OperationKind, RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions,
};
use crab_storage::{Store, StoreLayout};
use crab_write::catalog::publish_inventory;
use crab_xet::hash::MerkleHash;
use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    process::{Command, Stdio},
    sync::{Arc, atomic::AtomicBool},
};
use tokio_util::sync::CancellationToken;

fn git(directory: &std::path::Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.test")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.test")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[tokio::test]
async fn catalog_recovers_from_bad_evidence_without_a_local_repository() {
    let fixture = tempfile::tempdir().unwrap();
    git(fixture.path(), &["init", "--bare", "--quiet"], b"");
    let blob = String::from_utf8(git(
        fixture.path(),
        &["hash-object", "-w", "--stdin"],
        b"catalog payload\n",
    ))
    .unwrap();
    let tree_input = format!("100644 blob {}\tfile.txt\n", blob.trim());
    let tree = String::from_utf8(git(fixture.path(), &["mktree"], tree_input.as_bytes())).unwrap();
    let commit = String::from_utf8(git(
        fixture.path(),
        &["commit-tree", tree.trim(), "-m", "catalog"],
        b"",
    ))
    .unwrap();
    let wire = git(
        fixture.path(),
        &["pack-objects", "--stdout", "--revs"],
        commit.as_bytes(),
    );
    let expected = [
        ("blob", blob.trim()),
        ("tree", tree.trim()),
        ("commit", commit.trim()),
    ]
    .map(|(kind, oid)| {
        (
            gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap(),
            git(fixture.path(), &["cat-file", kind, oid], b""),
        )
    });
    let incoming = quarantine(
        wire.as_slice(),
        fixture.path(),
        ReceiveLimits {
            max_pack_bytes: 65536,
            max_objects: 8,
            max_object_bytes: 4096,
            max_inflated_bytes: 65536,
            max_delta_depth: 8,
        },
        || false,
        |_| Ok(None),
    )
    .unwrap();
    let prepared = incoming
        .prepare(fixture.path(), 65536, &AtomicBool::new(false))
        .unwrap()
        .unwrap();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "catalog".to_owned());
    let pack_id = prepared.content_hash().to_hex().to_string();
    for (source, target) in [
        (prepared.pack_path(), layout.pack_path(&pack_id)),
        (prepared.index_path(), layout.pack_index_path(&pack_id)),
        (
            prepared.reverse_path(),
            layout.pack_reverse_index_path(&pack_id),
        ),
        (
            prepared.kinds_path(),
            layout.pack_kind_metadata_path(&pack_id),
        ),
    ] {
        store
            .put(&target, Bytes::from(std::fs::read(source).unwrap()))
            .await
            .unwrap();
    }
    let pack = PackManifestEntry {
        pack_id: pack_id.clone(),
        content_hash: pack_id,
        size: prepared.size(),
        object_count: prepared.object_count().into(),
        ref_tips: vec![commit.trim().to_owned()],
    };
    let (index_hash, _, index) = compact_pack_index(1, std::slice::from_ref(&pack)).unwrap();
    manifest_store::upload_segmented_bulk(
        &store,
        &layout,
        &BulkData {
            shard_index: Default::default(),
            pack_index: index,
        },
    )
    .await
    .unwrap();
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    manifest.pack_index_hash = index_hash;
    manifest
        .refs
        .insert("refs/heads/main".to_owned(), commit.trim().to_owned());
    manifest.seal_git_validation();
    manifest_store::create_manifest(&store, &layout, &manifest)
        .await
        .unwrap();
    let mut other_pack = pack.clone();
    other_pack.pack_id = "f".repeat(64);
    let mismatched = crab_write::catalog::LocatorPackEvidence::from_local(
        &other_pack,
        prepared.index_path(),
        prepared.reverse_path(),
        &prepared.git_sha1().to_string(),
        None,
    )
    .unwrap();
    // Force the successful publisher to obtain all evidence from storage.
    drop(prepared);
    drop(incoming);
    drop(fixture);
    let anchor = GitLocatorCoverage {
        generation: 1,
        pack_index_hash: MerkleHash::from_hex(&manifest.pack_index_hash).unwrap(),
    };
    let index_path = layout.pack_index_path(&pack.pack_id);
    let (index_bytes, _) = store.get_with_etag(&index_path).await.unwrap();
    store.delete(&index_path).await.unwrap();
    store
        .put(&index_path, Bytes::from_static(b"truncated index"))
        .await
        .unwrap();
    let mut failed_writer = GitObjectLocatorWriter::open_for_publication(
        Arc::clone(store.inner()),
        layout.repo_prefix(),
        3,
    )
    .await
    .unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let cancelled_result = publish_inventory(
        &mut failed_writer,
        &store,
        &layout,
        &mut HashMap::new(),
        anchor,
        std::slice::from_ref(&pack),
        true,
        &cancelled,
    )
    .await;
    let mut local = HashMap::from([(MerkleHash::from_hex(&pack.pack_id).unwrap(), mismatched)]);
    let mismatch_result = publish_inventory(
        &mut failed_writer,
        &store,
        &layout,
        &mut local,
        anchor,
        std::slice::from_ref(&pack),
        true,
        &CancellationToken::new(),
    )
    .await;
    let failed = publish_inventory(
        &mut failed_writer,
        &store,
        &layout,
        &mut HashMap::new(),
        anchor,
        std::slice::from_ref(&pack),
        true,
        &CancellationToken::new(),
    )
    .await;
    let failed_coverage = failed_writer.coverage();
    failed_writer.close().await.unwrap();
    assert_eq!(
        (
            matches!(cancelled_result, Err(crab_write::WriteError::Cancelled)),
            matches!(
                mismatch_result,
                Err(crab_write::WriteError::CorruptObject { .. })
            ),
            failed.is_err(),
            failed_coverage
        ),
        (true, true, true, None)
    );
    store.delete(&index_path).await.unwrap();
    store.put(&index_path, index_bytes).await.unwrap();
    let mut writer = GitObjectLocatorWriter::open_for_publication(
        Arc::clone(store.inner()),
        layout.repo_prefix(),
        3,
    )
    .await
    .unwrap();
    let result = publish_inventory(
        &mut writer,
        &store,
        &layout,
        &mut HashMap::new(),
        GitLocatorCoverage {
            generation: 1,
            pack_index_hash: MerkleHash::from_hex(&manifest.pack_index_hash).unwrap(),
        },
        &[pack],
        true,
        &CancellationToken::new(),
    )
    .await;
    let close = writer.close().await;
    close.unwrap();
    assert!(result.unwrap().0);
    let visibility = GitVisibilityIndex::new(
        1,
        &manifest.pack_index_hash,
        &manifest.git_validation_digest,
        BTreeMap::from([(
            "refs/heads/main".to_owned(),
            expected.iter().map(|(oid, _)| oid.to_string()).collect(),
        )]),
    )
    .unwrap();
    git_visibility::upload_if_absent(&store, &layout, &visibility)
        .await
        .unwrap();
    let runtime = Arc::new(RemoteGitRuntime::default());
    let cancel = CancellationToken::new();
    let repository = RemoteGitRepository::open(
        store,
        layout,
        RepositoryIdentity::new("memory", "catalog", 1).unwrap(),
        runtime.clone(),
        RepositoryOptions::default(),
        &cancel,
    )
    .await
    .unwrap();
    let operation = repository
        .operation(OperationKind::Repository, &cancel)
        .await
        .unwrap();
    let result = async {
        let mut actual = Vec::new();
        for (oid, _) in &expected {
            actual.push((*oid, operation.read_object(*oid).await?.data.to_vec()));
        }
        Ok(actual)
    }
    .await;
    let actual = operation.finish(result).await;
    runtime.shutdown().await;
    assert_eq!(actual.unwrap(), expected.to_vec());
}
