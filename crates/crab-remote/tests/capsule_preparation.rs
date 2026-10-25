#![cfg(feature = "publication")]

use std::sync::Arc;

use bytes::Bytes;
use crab_remote::prepare::{ContentArtifact, ContentFile, PreparedContent};
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::{PushShardSession, file_info_from_placements, xorb_info_from_placements};
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use crab_xet::xorb::format::Chunk;
use tokio_util::sync::CancellationToken;

const LIMIT: u64 = 8 * 1024 * 1024;

async fn content_fixture() -> (tempfile::TempDir, PreparedContent) {
    let directory = tempfile::tempdir().unwrap();
    let data = Bytes::from_static(b"prepared capsule content must remain reconstructable");
    let chunk_hash = compute_data_hash(&data);
    let file_hash = MerkleHash::from(*blake3::hash(&data).as_bytes());
    let mut builder = XorbBuilder::new();
    builder
        .push(
            &Chunk {
                hash: chunk_hash,
                data: data.clone(),
            },
            RunId(0),
        )
        .unwrap();
    let xorb = builder.finalize().unwrap().pop().unwrap();
    let placements = xorb
        .placements
        .iter()
        .map(|placement| (placement.chunk_hash, placement.clone()))
        .collect();
    let file = file_info_from_placements(file_hash, &[chunk_hash], &placements).unwrap();
    let xorb_info = Arc::new(xorb_info_from_placements(xorb.hash, &xorb.placements).unwrap());
    let mut shards = PushShardSession::new();
    shards.add_file_bundle(file, &[xorb_info]).unwrap();
    let (shard, shard_hash) = shards.finalize().unwrap().pop().unwrap();
    let xorb_path = directory.path().join("content.xorb");
    let shard_path = directory.path().join("content.shard");
    std::fs::write(&xorb_path, &xorb.bytes).unwrap();
    std::fs::write(&shard_path, &shard).unwrap();
    let content = PreparedContent::new(
        vec![ContentArtifact::new(
            xorb.hash.into(),
            *blake3::hash(&xorb.bytes).as_bytes(),
            xorb.bytes.len() as u64,
            xorb_path,
        )],
        vec![ContentArtifact::new(
            shard_hash.into(),
            *blake3::hash(&shard).as_bytes(),
            shard.len() as u64,
            shard_path,
        )],
        vec![ContentFile::new(
            file_hash.into(),
            data.len() as u64,
            shard_hash.into(),
        )],
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    (directory, content)
}

#[tokio::test]
async fn prepared_catalog_preserves_raw_blake3_body_identity() {
    let (directory, content) = content_fixture().await;
    let expected = blake3::hash(&std::fs::read(directory.path().join("content.xorb")).unwrap())
        .to_hex()
        .to_string();
    let actual = content
        .pointer_catalog()
        .xorbs()
        .values()
        .next()
        .unwrap()
        .body_digest();
    assert_eq!(
        actual, expected,
        "body digests use raw Blake3 hex; only protocol identities use Xet word order"
    );
}

#[derive(Default)]
struct Reads(std::sync::atomic::AtomicUsize);
impl crab_storage::StorageObserver for Reads {
    fn started(&self, _: crab_storage::StorageOperation) {}
    fn finished(&self, observation: crab_storage::StorageObservation) {
        if matches!(
            observation.operation,
            crab_storage::StorageOperation::Get | crab_storage::StorageOperation::Range
        ) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

fn git(directory: &std::path::Path, args: &[&str]) -> Vec<u8> {
    git_input(directory, args, b"")
}

fn git_input(directory: &std::path::Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn source_commit(directory: &std::path::Path, content: &PreparedContent) {
    git(directory, &["init", "-b", "main"]);
    let file = &content.files()[0];
    let pointer = format!(
        "version https://crab.build/spec/v1\nfile-hash {}\nsize {}\n",
        blake3::Hash::from_bytes(*file.file_hash()).to_hex(),
        file.size()
    );
    std::fs::write(directory.join("model.bin"), pointer).unwrap();
    git(directory, &["add", "model.bin"]);
    git(directory, &["commit", "-m", "publish pointer"]);
}

async fn layout() -> (crab_storage::StoreLayout<crab_storage::Store>, Arc<Reads>) {
    let reads = Arc::new(Reads::default());
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(reads.clone());
    let layout = crab_storage::StoreLayout::new(store, "repositories/prepared".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    (layout, reads)
}

fn proof_limits() -> crab_read::dependency_proof::DependencyProofLimits {
    crab_read::dependency_proof::DependencyProofLimits {
        max_dependencies: 16,
        max_total_file_bytes: LIMIT,
        max_duration: std::time::Duration::from_secs(60),
        lookup: crab_metadata::file_index_lookup::FileIndexLookupLimits {
            max_files: 16,
            max_shard_visits: 16,
            max_shard_bytes: LIMIT,
            max_recipe_entries: 1024,
        },
        content: crab_read::pointer_proof::PointerProofLimits {
            max_file_bytes: LIMIT,
            max_shard_bytes: LIMIT,
            max_xorb_bytes: LIMIT,
            max_read_bytes: LIMIT,
            max_chunks: 1024,
            max_duration: std::time::Duration::from_secs(60),
        },
    }
}

async fn prepare(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    directory: &std::path::Path,
    content: Option<PreparedContent>,
) -> (
    crab_remote::prepare::Prepared,
    crab_read::capsule_protocol::CapsuleRepositoryView,
    Arc<crab_remote_git::RemoteGitRuntime>,
    crab_remote::publication::PublicationLeases,
) {
    let oid = gix_hash::ObjectId::from_hex(
        String::from_utf8(git(directory, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .as_bytes(),
    )
    .unwrap();
    prepare_update(layout, directory, content, "refs/heads/main", Some(oid)).await
}

async fn prepare_update(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    directory: &std::path::Path,
    content: Option<PreparedContent>,
    ref_name: &str,
    new: Option<gix_hash::ObjectId>,
) -> (
    crab_remote::prepare::Prepared,
    crab_read::capsule_protocol::CapsuleRepositoryView,
    Arc<crab_remote_git::RemoteGitRuntime>,
    crab_remote::publication::PublicationLeases,
) {
    let cancel = CancellationToken::new();
    let leases = crab_remote::publication::acquire_leases(
        layout.store(),
        layout,
        [ref_name.to_owned()],
        crab_remote::publication::LeaseOptions::renewing(std::time::Duration::from_secs(60)),
        &cancel,
    )
    .await
    .unwrap();
    let root = crab_write::capsule_protocol::open_root(layout)
        .await
        .unwrap();
    let view = crab_read::capsule_protocol::open_git_view_from_root(
        layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: LIMIT,
            max_frontier_bytes: LIMIT,
        },
    )
    .await
    .unwrap();
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let repository = view
        .git_repository_from_store(
            layout.clone(),
            crab_remote_git::RepositoryIdentity::new("memory", "prepared", 1).unwrap(),
            runtime.clone(),
            crab_remote_git::RepositoryOptions::default(),
            LIMIT,
            &cancel,
            None,
        )
        .await
        .unwrap();
    let incoming = if let Some(oid) = new {
        let mut revisions = format!("{oid}\n");
        for old in view.refs().values() {
            revisions.push_str(&format!("^{old}\n"));
        }
        let pack = git_input(
            directory,
            &["pack-objects", "--stdout", "--revs"],
            revisions.as_bytes(),
        );
        let incoming = directory.join("incoming.pack");
        std::fs::write(&incoming, pack).unwrap();
        Some(std::io::BufReader::new(
            std::fs::File::open(incoming).unwrap(),
        ))
    } else {
        None
    };
    let limits = crab_git::incoming_pack::ReceiveLimits {
        max_pack_bytes: LIMIT,
        max_objects: 1024,
        max_object_bytes: LIMIT as usize,
        max_inflated_bytes: LIMIT,
        max_delta_depth: 128,
    };
    let prepared = crab_remote::prepare::prepare_capsule(
        repository,
        view.git_visibility_index().await.unwrap(),
        crab_git::incoming_pack::ScratchSpace::new(directory, limits),
        incoming,
        vec![crab_git::receive_plan::RefUpdate {
            name: ref_name.to_owned(),
            old: view
                .refs()
                .get(ref_name)
                .map(|oid| gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap()),
            new,
        }],
        std::collections::BTreeMap::new(),
        &cancel,
        crab_remote::prepare::Options {
            layout: layout.clone(),
            graph: crab_git::receive_plan::GraphLimits {
                max_ref_updates: 16,
                max_graph_steps: 1024,
                max_object_bytes: LIMIT as usize,
                max_read_bytes: LIMIT,
            },
            pack: limits,
            policy: |_: &str| crab_git::receive_plan::RefPolicy {
                allow_delete: true,
                allow_non_fast_forward: false,
            },
        },
    )
    .await
    .unwrap();
    let prepared = match content {
        Some(content) => prepared.attach_content(content).unwrap(),
        None => prepared,
    };
    (prepared, view, runtime, leases)
}

#[tokio::test(flavor = "multi_thread")]
async fn capsule_upload_rejects_changed_prepared_bytes() {
    let mut rejected = Vec::new();
    for artifact in ["content.xorb", "content.shard"] {
        let (directory, content) = content_fixture().await;
        source_commit(directory.path(), &content);
        let (layout, _) = layout().await;
        let (prepared, view, runtime, leases) =
            prepare(&layout, directory.path(), Some(content)).await;
        std::fs::write(directory.path().join(artifact), b"changed after inspection").unwrap();
        let result = prepared
            .upload_capsule(&view, proof_limits(), &CancellationToken::new())
            .await;
        leases.release().await;
        runtime.shutdown().await;
        rejected.push((artifact, result.is_err()));
    }
    assert!(
        rejected.iter().all(|(_, failed)| *failed),
        "changed artifacts must fail before ref publication: {rejected:?}"
    );
}

async fn verify_indexed_publication(
    layout: crab_storage::StoreLayout<crab_storage::Store>,
    occupied: bool,
) {
    let (directory, content) = content_fixture().await;
    source_commit(directory.path(), &content);
    let chunk = content
        .pointer_catalog()
        .xorbs()
        .values()
        .next()
        .unwrap()
        .chunks()[0]
        .hash()
        .to_owned();
    if occupied {
        let artifact = &content.xorbs()[0];
        layout
            .store()
            .put(
                &layout.xorb_path(&MerkleHash::from(*artifact.protocol_hash())),
                Bytes::from(std::fs::read(directory.path().join("content.xorb")).unwrap()),
            )
            .await
            .unwrap();
    }
    let (prepared, view, runtime, leases) = prepare(&layout, directory.path(), Some(content)).await;
    let artifacts = prepared
        .upload_capsule(&view, proof_limits(), &CancellationToken::new())
        .await
        .unwrap();
    let result = artifacts
        .commit(
            None,
            std::time::Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await;
    leases.release_after_commit().await;
    runtime.shutdown().await;
    assert!(matches!(
        result.unwrap(),
        crab_remote::prepare::CapsuleCommitOutcome::Committed { .. }
    ));
    let hash = MerkleHash::from_hex(&chunk).unwrap();
    let candidates = crab_metadata::capsule_protocol::CapsuleChunkIndex::new(&layout)
        .candidates(layout.store(), &[hash], LIMIT)
        .await
        .expect("shared publication must populate cold native-add candidates");
    let placement = candidates.placements[&<[u8; 32]>::from(hash)].clone();
    let anchor = candidates.anchors[&placement.source_anchor_id].clone();
    let path = layout.xorb_path(&MerkleHash::from(placement.xorb_hash));
    let (body, identity) = layout.store().get_with_etag(&path).await.unwrap();
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let current = crab_read::capsule_protocol::open_git_view_from_root(
        &layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: LIMIT,
            max_frontier_bytes: LIMIT,
        },
    )
    .await
    .unwrap();
    let catalog = current.pointer_catalog().await.unwrap();
    anchor
        .validate_placement(&placement, &catalog, path.as_ref())
        .unwrap();
    assert_eq!(
        (
            anchor.origin.etag,
            anchor.origin.object_version,
            anchor.body_digest
        ),
        (
            identity.e_tag,
            identity.version,
            blake3::hash(&body).to_hex().to_string()
        )
    );
    let crab_git::pointer_detect::PointerKind::Crab(pointer) = crab_git::pointer_detect::classify(
        &std::fs::read(directory.path().join("model.bin")).unwrap(),
    ) else {
        panic!("fixture pointer");
    };
    crab_read::pointer_proof::verify_crab_pointer(
        &layout,
        &pointer,
        MerkleHash::from_hex(catalog.files().values().next().unwrap().shard_hash()).unwrap(),
        proof_limits().content,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn capsule_commit_indexes_verified_native_payload() {
    for occupied in [false, true] {
        let (layout, _) = layout().await;
        verify_indexed_publication(layout, occupied).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires CRAB_E2E_FLAT_BUCKET and explicit scratch S3 credentials"]
async fn live_capsule_preparation_publishes_and_reconstructs_native_content() {
    let bucket = std::env::var("CRAB_E2E_FLAT_BUCKET").expect("explicit scratch bucket required");
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    for occupied in [false, true] {
        let prefix = format!("shared-preparation/{nonce}/{occupied}");
        let layout = crab_storage::StoreLayout::with_global_prefix(
            store.clone(),
            format!("{prefix}/repo"),
            format!("{prefix}/global"),
        );
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        verify_indexed_publication(layout, occupied).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn git_only_capsule_upload_does_not_read_historical_catalog() {
    let (directory, content) = content_fixture().await;
    source_commit(directory.path(), &content);
    let (layout, reads) = layout().await;
    let (prepared, view, runtime, leases) = prepare(&layout, directory.path(), Some(content)).await;
    let artifacts = prepared
        .upload_capsule(&view, proof_limits(), &CancellationToken::new())
        .await
        .unwrap();
    let committed = artifacts
        .commit(
            None,
            std::time::Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        committed,
        crab_remote::prepare::CapsuleCommitOutcome::Committed { .. }
    ));
    leases.release_after_commit().await;
    runtime.shutdown().await;
    git(directory.path(), &["rm", "model.bin"]);
    std::fs::write(directory.path().join("README.md"), b"ordinary Git update").unwrap();
    git(directory.path(), &["add", "README.md"]);
    git(
        directory.path(),
        &["commit", "-m", "replace pointer with ordinary Git file"],
    );
    let (prepared, view, runtime, leases) = prepare(&layout, directory.path(), None).await;
    assert!(prepared.plan().pointers().is_empty());
    reads.0.store(0, std::sync::atomic::Ordering::Relaxed);
    let result = prepared
        .upload_capsule(&view, proof_limits(), &CancellationToken::new())
        .await;
    let count = reads.0.load(std::sync::atomic::Ordering::Relaxed);
    leases.release().await;
    runtime.shutdown().await;
    result.unwrap();
    assert_eq!(
        count, 0,
        "ordinary Git publication must not read unrelated Xet history"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn committed_capsule_survives_candidate_index_failure() {
    let (directory, content) = content_fixture().await;
    source_commit(directory.path(), &content);
    let file = content.files()[0].clone();
    let (layout, _) = layout().await;
    // Inject the failure in the capsule owner. A retired manifest database
    // no longer participates in this publication or proves a repair boundary.
    layout
        .store()
        .put_exact(
            &layout.capsule_chunk_index_path(),
            Bytes::from_static(b"invalid capsule hint root"),
        )
        .await
        .unwrap();
    let (prepared, view, runtime, leases) = prepare(&layout, directory.path(), Some(content)).await;
    let artifacts = prepared
        .upload_capsule(&view, proof_limits(), &CancellationToken::new())
        .await
        .unwrap();
    let result = artifacts
        .commit(
            None,
            std::time::Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await;
    leases.release_after_commit().await;
    runtime.shutdown().await;
    assert!(
        matches!(
            result.unwrap(),
            crab_remote::prepare::CapsuleCommitOutcome::Committed { .. }
        ),
        "repairable hint-index failure must not reject committed refs"
    );
    assert!(
        crab_metadata::capsule_protocol::CapsuleChunkIndex::new(&layout)
            .available(layout.store(), LIMIT)
            .await
            .is_err()
    );
    let tip = String::from_utf8(git(directory.path(), &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(
        crab_read::capsule_protocol::read_visible_refs(&layout)
            .await
            .unwrap()
            .get("refs/heads/main"),
        Some(&tip)
    );
    let crab_git::pointer_detect::PointerKind::Crab(pointer) = crab_git::pointer_detect::classify(
        &std::fs::read(directory.path().join("model.bin")).unwrap(),
    ) else {
        panic!("fixture pointer");
    };
    crab_read::pointer_proof::verify_crab_pointer(
        &layout,
        &pointer,
        MerkleHash::from(*file.shard_hash()),
        proof_limits().content,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn historical_pointer_lookup_preserves_declared_size_checks() {
    for size_delta in [0, 1] {
        let (directory, content) = content_fixture().await;
        let file = content.files()[0].clone();
        source_commit(directory.path(), &content);
        let (layout, _) = layout().await;
        let (prepared, view, runtime, leases) =
            prepare(&layout, directory.path(), Some(content)).await;
        let artifacts = prepared
            .upload_capsule(&view, proof_limits(), &CancellationToken::new())
            .await
            .unwrap();
        let committed = artifacts
            .commit(
                None,
                std::time::Duration::from_secs(60),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            committed,
            crab_remote::prepare::CapsuleCommitOutcome::Committed { .. }
        ));
        leases.release_after_commit().await;
        runtime.shutdown().await;
        // A hint changes the Git blob identity while retaining the existing file.
        // Native negotiation therefore sends a real historical-pointer dependency.
        let pointer = format!(
            "version https://crab.build/spec/v1\nfile-hash {}\nsize {}\nshard-hint {}\n",
            blake3::Hash::from_bytes(*file.file_hash()).to_hex(),
            file.size() + size_delta,
            blake3::Hash::from_bytes(*file.shard_hash()).to_hex()
        );
        std::fs::write(directory.path().join("model.bin"), pointer).unwrap();
        git(directory.path(), &["add", "model.bin"]);
        git(
            directory.path(),
            &["commit", "-m", "update pointer binding"],
        );
        let (prepared, view, runtime, leases) = prepare(&layout, directory.path(), None).await;
        assert_eq!(prepared.plan().pointers().len(), 1);
        let result = prepared
            .upload_capsule(&view, proof_limits(), &CancellationToken::new())
            .await;
        leases.release().await;
        runtime.shutdown().await;
        assert_eq!(
            result.is_ok(),
            size_delta == 0,
            "historical pointer size delta {size_delta}"
        );
    }
}

#[path = "capsule_preparation/ref_visibility.rs"]
mod ref_visibility;

#[path = "capsule_preparation/cancellation.rs"]
mod cancellation;

#[path = "capsule_preparation/lfs.rs"]
mod lfs;

#[tokio::test(flavor = "multi_thread")]
async fn protected_preparation_stages_a_native_capsule_without_publishing_refs() {
    let (directory, content) = content_fixture().await;
    source_commit(directory.path(), &content);
    let (layout, _) = layout().await;
    let (prepared, view, runtime, leases) = prepare(&layout, directory.path(), Some(content)).await;
    let upload_prefix = format!("{}/staging/{}", layout.repo_prefix(), "7".repeat(32));
    let staged_layout = crab_storage::StoreLayout::new(
        layout
            .store()
            .clone()
            .with_staging_writes(upload_prefix.clone()),
        layout.repo_prefix().to_owned(),
    );
    let prepared = prepared.with_staging_layout(staged_layout).unwrap();
    let target = String::from_utf8(git(directory.path(), &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    let updates = vec![crab_auth::PushRefUpdate {
        ref_name: "refs/heads/main".to_owned(),
        old_oid: None,
        new_oid: target,
    }];
    let plan = prepared
        .upload_capsule(&view, proof_limits(), &CancellationToken::new())
        .await
        .unwrap()
        .stage_protected_plan(
            &"7".repeat(32),
            &upload_prefix,
            updates.clone(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let run_key = layout.capsule_path(&plan.run_hash).to_string();
    let staged = plan
        .staged_objects
        .iter()
        .find(|object| object.canonical_key == run_key)
        .unwrap();
    let (bytes, _) = layout
        .store()
        .get_with_etag(&object_store::path::Path::from(staged.staged_key.clone()))
        .await
        .unwrap();
    let run = crab_metadata::capsule_protocol::CapsuleRun::decode(bytes).unwrap();
    let root = crab_metadata::capsule_protocol::load_root(&layout)
        .await
        .unwrap();
    leases.release().await;
    runtime.shutdown().await;
    assert!(
        plan.schema_version == crab_remote::protected::PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION
            && plan.base_root_digest == view.root().digest()
            && plan.ref_updates == updates
            && run.transaction_ids() == [plan.transaction_id]
            && root.record().root().refs().is_empty()
    );
}
