use super::*;

fn source(body: &[u8], oid: &str) -> (tempfile::TempDir, PreparedContent, crab_git::LfsPointer) {
    let directory = tempfile::tempdir().unwrap();
    let pointer = crab_git::LfsPointer::parse(
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            body.len()
        )
        .as_bytes(),
    )
    .unwrap();
    let payload = directory.path().join("payload");
    std::fs::write(&payload, body).unwrap();
    let content = PreparedContent::from_lfs(
        vec![ContentArtifact::new(
            pointer.oid,
            *blake3::hash(body).as_bytes(),
            body.len() as u64,
            payload,
        )],
        LIMIT,
    )
    .unwrap();
    git(directory.path(), &["init", "-b", "main"]);
    // Imported trees may retain an explicit zero-size pointer; canonical clean
    // instead stores an empty file directly as Git bytes.
    std::fs::write(
        directory.path().join("asset.lfs"),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            body.len(),
        ),
    )
    .unwrap();
    git(directory.path(), &["add", "asset.lfs"]);
    git(directory.path(), &["commit", "-m", "LFS pointer"]);
    (directory, content, pointer)
}

const ABC_OID: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const EMPTY_OID: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

struct OriginProof {
    size: u64,
    wrote: std::sync::atomic::AtomicBool,
    read_after_write: std::sync::atomic::AtomicBool,
}

impl crab_storage::StorageObserver for OriginProof {
    fn started(&self, _: crab_storage::StorageOperation) {}
    fn finished(&self, event: crab_storage::StorageObservation) {
        use crab_storage::{StorageOperation as Op, StorageOutcome};
        use std::sync::atomic::Ordering::Relaxed;
        if event.outcome != StorageOutcome::Success {
            return;
        }
        if matches!(event.operation, Op::MultipartComplete | Op::Put) {
            self.wrote.store(true, Relaxed);
        }
        if event.operation == Op::Get && event.bytes_read == self.size && self.wrote.load(Relaxed) {
            self.read_after_write.store(true, Relaxed);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lfs_payload_and_recovery_are_durable_before_refs() {
    for (body, oid) in [(b"abc".as_slice(), ABC_OID), (b"".as_slice(), EMPTY_OID)] {
        let (directory, content, pointer) = source(body, oid);
        let (layout, _) = layout().await;
        let proof = Arc::new(OriginProof {
            size: body.len() as u64,
            wrote: false.into(),
            read_after_write: false.into(),
        });
        let layout = crab_storage::StoreLayout::new(
            layout.store().clone().with_storage_observer(proof.clone()),
            layout.repo_prefix().to_owned(),
        );
        let (prepared, view, runtime, leases) =
            prepare(&layout, directory.path(), Some(content)).await;
        let plan = "a".repeat(64);
        prepared
            .stage_recovery_artifacts(&plan, &CancellationToken::new())
            .await
            .unwrap();
        let recovery = layout
            .store()
            .get_with_etag(&layout.capsule_plan_recovery_lfs_path(&plan, oid))
            .await
            .unwrap()
            .0;
        let hidden = crab_metadata::capsule_protocol::load_root(&layout)
            .await
            .unwrap();
        proof
            .wrote
            .store(false, std::sync::atomic::Ordering::Relaxed);
        proof
            .read_after_write
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let artifacts = prepared
            .upload_capsule(&view, proof_limits(), &CancellationToken::new())
            .await
            .unwrap();
        let verified_origin = proof
            .read_after_write
            .load(std::sync::atomic::Ordering::Relaxed);
        let committed = artifacts
            .commit(
                None,
                std::time::Duration::from_secs(30),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let remote = crab_lfs::LfsObjectStore::new(layout.store().clone(), layout.repo_prefix());
        let published = remote.get(&pointer.oid).await.unwrap();
        let root = crab_metadata::capsule_protocol::load_root(&layout)
            .await
            .unwrap();
        let published_view = crab_read::capsule_protocol::open_git_view_from_root(
            &layout,
            root,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: LIMIT,
                max_frontier_bytes: LIMIT,
            },
        )
        .await
        .unwrap();
        leases.release().await;
        runtime.shutdown().await;
        assert!(
            recovery == body
                && verified_origin
                && published == body
                && hidden.record().root().refs().is_empty()
                && !published_view.refs().is_empty()
                && matches!(
                    committed,
                    crab_remote::prepare::CapsuleCommitOutcome::Committed { .. }
                )
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_lfs_snapshot_cannot_publish_a_ref() {
    let (directory, content, _) = source(b"abc", ABC_OID);
    let (layout, _) = layout().await;
    let (prepared, view, runtime, leases) = prepare(&layout, directory.path(), Some(content)).await;
    std::fs::write(directory.path().join("payload"), b"def").unwrap();
    let result = prepared
        .upload_capsule(&view, proof_limits(), &CancellationToken::new())
        .await;
    let root = crab_metadata::capsule_protocol::load_root(&layout)
        .await
        .unwrap();
    leases.release().await;
    runtime.shutdown().await;
    assert!(
        matches!(result, Err(crab_remote::prepare::Error::Dependency(_)))
            && root.record().root().refs().is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn protected_lfs_payload_stays_in_the_verified_staging_inventory() {
    let (directory, content, pointer) = source(b"abc", ABC_OID);
    let (layout, _) = layout().await;
    let (prepared, view, runtime, leases) = prepare(&layout, directory.path(), Some(content)).await;
    let upload_prefix = format!("{}/staging/{}", layout.repo_prefix(), "8".repeat(32));
    let staged = crab_storage::StoreLayout::new(
        layout
            .store()
            .clone()
            .with_staging_writes(upload_prefix.clone()),
        layout.repo_prefix().to_owned(),
    );
    let target = String::from_utf8(git(directory.path(), &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    let plan = prepared
        .with_staging_layout(staged)
        .unwrap()
        .upload_capsule(&view, proof_limits(), &CancellationToken::new())
        .await
        .unwrap()
        .stage_protected_plan(
            &"8".repeat(32),
            &upload_prefix,
            vec![crab_auth::PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: None,
                new_oid: target,
            }],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let path = crab_lfs::LfsObjectStore::object_path_for_prefix(layout.repo_prefix(), &pointer.oid);
    let object = plan
        .staged_objects
        .iter()
        .find(|object| object.canonical_key == path.as_ref())
        .unwrap();
    let payload = layout
        .store()
        .get_with_etag(&object_store::path::Path::from(object.staged_key.clone()))
        .await
        .unwrap()
        .0;
    let canonical = layout.store().head(&path).await;
    let root = crab_metadata::capsule_protocol::load_root(&layout)
        .await
        .unwrap();
    leases.release().await;
    runtime.shutdown().await;
    assert!(
        payload == b"abc".as_slice()
            && plan.staged_objects.len() == 2
            && matches!(canonical, Err(crab_storage::StorageError::NotFound { .. }))
            && root.record().root().refs().is_empty()
    );
}
