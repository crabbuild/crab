use super::*;
use crab_git::incoming_pack::{ExternalDeltaBase, IncomingPack, ReceiveLimits};
use object_store::ObjectStoreExt;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

async fn publish_dependent_blob(
    layout: &StoreLayout<Store>,
    name: &str,
    base: &[u8],
    target: &[u8],
) {
    let kind = gix_object::Kind::Blob;
    let base_oid = crab_remote::objects::object_id(kind, base).unwrap();
    let target_oid = crab_remote::objects::object_id(kind, target).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let incoming = IncomingPack::from_generated_objects(
        [(kind, target.to_vec())],
        directory.path(),
        ReceiveLimits {
            max_pack_bytes: LIMIT,
            max_objects: 2,
            max_object_bytes: LIMIT as usize,
            max_inflated_bytes: LIMIT,
            max_delta_depth: 8,
        },
        || false,
    )
    .unwrap();
    let prepared = incoming
        .prepare_with_external_delta_bases(
            directory.path(),
            LIMIT,
            &AtomicBool::new(false),
            &BTreeMap::from([(target_oid, base_oid)]),
            &BTreeMap::from([(
                target_oid,
                ExternalDeltaBase::new(base_oid, kind, base.to_vec(), 0),
            )]),
            8,
            LIMIT as usize,
        )
        .unwrap()
        .unwrap();
    assert_eq!(prepared.external_delta_bases(), &[base_oid]);
    let pack = CapsuleGitPack::new_with_external_delta_bases(
        Bytes::from(std::fs::read(prepared.pack_path()).unwrap()),
        Bytes::from(std::fs::read(prepared.index_path()).unwrap()),
        Bytes::from(std::fs::read(prepared.reverse_path()).unwrap()),
        Bytes::from(std::fs::read(prepared.kinds_path()).unwrap()),
        prepared.git_sha1().to_string(),
        u64::from(prepared.object_count()),
        vec![base_oid.to_string()],
    )
    .unwrap();
    let root = crab_write::capsule_protocol::open_root(layout)
        .await
        .unwrap();
    let ref_name = format!("refs/tags/{name}");
    let oid = target_oid.to_string();
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(
            &ref_name,
            None,
            Some(oid.clone()),
            None,
        )],
    )
    .unwrap();
    let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
        ref_name,
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
    crab_write::capsule_protocol::publish(layout, root, &transaction, &capsule)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_capsule_install_repairs_uncheckpointed_delta_chains_and_reuses_content_names() {
    let (layout, _) = empty_fixture().await;
    let base = vec![b'a'; 32 * 1024];
    let mut first = base.clone();
    first[1024] = b'b';
    let mut second = first.clone();
    second[2048] = b'c';
    publish_blobs(&layout, &[("z-base", &base)]).await;
    publish_dependent_blob(&layout, "m-first", &base, &first).await;
    publish_dependent_blob(&layout, "a-second", &first, &second).await;
    let view = view(&layout).await;
    assert!(view.layered_checkpoint().is_none());
    let directory = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(directory.path()).unwrap();
    let original_packs = view
        .capsules()
        .iter()
        .flat_map(|capsule| capsule.git_packs())
        .filter(|pack| !pack.external_delta_bases().is_empty())
        .count();
    assert_eq!(original_packs, 2);
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let cancel = CancellationToken::new();
    let reader = view
        .git_repository_from_store(
            layout.clone(),
            crab_remote_git::RepositoryIdentity::new("memory", "delta-reader", 1).unwrap(),
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            LIMIT,
            &cancel,
        )
        .await
        .unwrap();
    let operation = reader
        .operation(crab_remote_git::OperationKind::Repository, &cancel)
        .await
        .unwrap();
    let oids = ["a-second", "m-first", "z-base"].map(|name| {
        gix_hash::ObjectId::from_hex(view.refs()[&format!("refs/tags/{name}")].as_bytes()).unwrap()
    });
    let objects = operation.read_objects(&oids).await;
    let objects = operation.finish(objects).await;
    runtime.shutdown().await;
    for (actual, expected) in objects.unwrap().iter().zip([&second, &first, &base]) {
        assert_eq!(actual.data.as_ref(), expected);
    }
    let mut prior = None;
    for _ in 0..2 {
        let installed = crab_read::capsule_protocol::install_git_packs_from_store(
            &view,
            &layout,
            directory.path(),
            LIMIT,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let paths = installed
            .paths
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(paths.len(), 3);
        if let Some(prior) = prior.replace(paths.clone()) {
            assert_eq!(paths, prior);
        }
        for path in paths {
            let body = std::fs::read(&path).unwrap();
            assert_eq!(
                path.file_name().unwrap().to_str().unwrap(),
                format!("pack-{}.pack", blake3::hash(&body).to_hex())
            );
        }
        for (name, expected) in [
            ("z-base", &base),
            ("m-first", &first),
            ("a-second", &second),
        ] {
            let oid = &view.refs()[&format!("refs/tags/{name}")];
            let output = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(directory.path())
                .args(["cat-file", "blob", oid])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, *expected);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_install_rejects_missing_or_cyclic_bases_before_installing_any_pack() {
    for cyclic in [false, true] {
        let (layout, _) = empty_fixture().await;
        publish_blob(&layout, "unrelated").await;
        let first = vec![b'a'; 32 * 1024];
        let mut second = first.clone();
        second[1024] = b'b';
        publish_dependent_blob(&layout, "first", &second, &first).await;
        if cyclic {
            publish_dependent_blob(&layout, "second", &first, &second).await;
        }
        let view = view(&layout).await;
        let directory = tempfile::tempdir().unwrap();
        crab_git::initialize_bare_git_dir(directory.path()).unwrap();
        let result = crab_read::capsule_protocol::install_git_packs_from_store(
            &view,
            &layout,
            directory.path(),
            LIMIT,
            None,
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(crab_read::ReadError::CorruptObject { .. })
        ));
        assert!(
            std::fs::read_dir(directory.path().join("objects/pack"))
                .unwrap()
                .next()
                .is_none()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_base_repack_verifies_bytes_and_fails_closed_on_corruption_or_cancellation() {
    for scenario in ["valid", "corrupt-base", "cancel-base"] {
        let (layout, observations) = empty_fixture().await;
        let base = (0..8192_u32)
            .flat_map(|ordinal| *blake3::hash(&ordinal.to_le_bytes()).as_bytes())
            .collect::<Vec<_>>();
        publish_blobs(&layout, &[("base", &base)]).await;
        checkpoint(&layout).await;
        let mut first = base.clone();
        first[base.len() / 2] ^= 1;
        let mut second = base.clone();
        second[base.len() / 2] ^= 2;
        publish_dependent_blob(&layout, "first", &base, &first).await;
        publish_dependent_blob(&layout, "second", &base, &second).await;
        let before = checkpoint(&layout).await;
        let sources = before.layered_checkpoint().unwrap().sources();
        assert_eq!(sources.len(), 3);
        let stable = sources[0].clone();
        assert!(
            sources[1..]
                .iter()
                .flat_map(|source| source.members())
                .all(|member| !member.external_delta_bases().is_empty())
        );
        let cancel = CancellationToken::new();
        if scenario == "corrupt-base" {
            let path = layout.capsule_path(stable.object_hash());
            let (bytes, _) = layout.store().get_with_etag(&path).await.unwrap();
            let mut corrupt = bytes.to_vec();
            // Corrupt the first object's entry, not the pack header outside a
            // selective base read. The authenticated entry must be rejected.
            corrupt[stable.members()[0].pack().offset() as usize + 12] ^= 1;
            layout
                .store()
                .inner()
                .put(&path, Bytes::from(corrupt).into())
                .await
                .unwrap();
        } else if scenario == "cancel-base" {
            *observations.1.lock().unwrap() = Some(cancel.clone());
        }
        observations.0.lock().unwrap().clear();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
                &layout,
                before.root_snapshot().clone(),
                LIMIT,
                &cancel,
            ),
        )
        .await
        .expect("external-base maintenance must return after cancellation or failure");
        *observations.1.lock().unwrap() = None;
        if scenario != "valid" {
            if scenario == "cancel-base" {
                assert!(
                    cancel.is_cancelled(),
                    "the base-read cancellation hook must run"
                );
            }
            assert!(result.is_err(), "{scenario}");
            let current = crab_write::capsule_protocol::open_root(&layout)
                .await
                .unwrap();
            assert_eq!(
                current.record().digest(),
                before.root().digest(),
                "{scenario}"
            );
            continue;
        }
        assert!(result.unwrap().published);
        let after = view(&layout).await;
        let sources = after.layered_checkpoint().unwrap().sources();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0], stable);
        assert_eq!(sources[1].members()[0].object_count(), 2);
        assert_eq!(sources[1].members()[0].external_delta_bases().len(), 1);
        assert_eq!(after.refs(), before.refs());
        verify_git_blobs(
            &layout,
            &after,
            &[("base", &base), ("first", &first), ("second", &second)],
        )
        .await;
    }
}
