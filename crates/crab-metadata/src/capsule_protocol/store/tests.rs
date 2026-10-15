#![expect(clippy::unwrap_used, reason = "test assertions")]

use super::*;
use crate::capsule_protocol::{
    CapsuleRefEdit, CapsuleRefState, CapsuleTransaction, RepositoryRoot,
};
use std::sync::Arc;

async fn run_fixture() -> (
    StoreLayout<Store>,
    crate::capsule_protocol::CapsulePointer,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
    let layout = StoreLayout::new(store, "repositories/run-pointer".to_owned());
    let transaction = CapsuleTransaction::new(
        &"1".repeat(64),
        vec![CapsuleRefEdit::new(
            "refs/tags/example",
            None,
            Some("2".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let run = CapsuleRun::leaf(Capsule::build(&transaction, vec![], vec![]).unwrap()).unwrap();
    layout
        .store()
        .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
    let pointer = crate::capsule_protocol::CapsulePointer::new(
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
    (layout, pointer, reads)
}

#[tokio::test]
async fn run_pointer_binding_is_identical_for_full_and_control_reads() {
    let (layout, pointer, _) = run_fixture().await;
    load_capsule_run(&layout, &pointer).await.unwrap();
    load_capsule_run_control(&layout, &pointer).await.unwrap();
    load_capsule_run_git_control(&layout, &pointer)
        .await
        .unwrap();
    for field in ["footer_hash", "control_boundary"] {
        let mut changed = serde_json::to_value(&pointer).unwrap();
        if field == "footer_hash" {
            changed[field] = serde_json::json!("f".repeat(64));
        } else {
            changed["control_offset"] = (pointer.control_offset() + 1).into();
            changed["control_size"] = (pointer.control_size() - 1).into();
        }
        let changed = serde_json::from_value(changed).unwrap();
        assert!(
            load_capsule_run(&layout, &changed).await.is_err(),
            "full read: {field}"
        );
        assert!(
            load_capsule_run_control(&layout, &changed).await.is_err(),
            "control read: {field}"
        );
        assert!(
            load_capsule_run_git_control(&layout, &changed)
                .await
                .is_err(),
            "paged Git control: {field}"
        );
    }
}

#[tokio::test]
async fn run_control_includes_verified_member_admission_in_one_read() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for count in [1, 2] {
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
            .with_read_request_observer(Arc::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
            }));
        let layout = StoreLayout::new(store, "repositories/control-admission".to_owned());
        let pack = crate::capsule_protocol::CapsuleGitPack::new(
            bytes::Bytes::from_static(b"PACK"),
            bytes::Bytes::from_static(b"index"),
            bytes::Bytes::from_static(b"reverse"),
            bytes::Bytes::from_static(b"locator"),
            "3".repeat(40),
            1,
        )
        .unwrap();
        let mut leaves = (1..=count)
            .map(|sequence| {
                let transaction = CapsuleTransaction::new(
                    &"1".repeat(64),
                    vec![CapsuleRefEdit::new(
                        "refs/heads/main",
                        None,
                        Some(format!("{sequence:040x}")),
                        None,
                    )],
                )
                .unwrap();
                let capsule = Capsule::build(&transaction, vec![pack.clone()], vec![]).unwrap();
                CapsuleRun::leaf_with_member_oids(capsule, vec![vec![[7; 20]]]).unwrap()
            })
            .collect::<Vec<_>>();
        let run = if count == 1 {
            leaves.remove(0)
        } else {
            CapsuleRun::compact(leaves).unwrap()
        };
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let pointer = crate::capsule_protocol::CapsulePointer::new(
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
        let (control, capsules) = load_capsule_run_control(&layout, &pointer).await.unwrap();
        assert_eq!(control.admission(), run.admission());
        assert_eq!(control.git_packs(), run.git_packs());
        assert_eq!(capsules.len(), count);
        assert_eq!(reads.load(Ordering::SeqCst), 1, "run with {count} capsules");
        for offset in [pointer.control_offset() - 1, pointer.control_offset() + 1] {
            let mut changed = serde_json::to_value(&pointer).unwrap();
            changed["control_offset"] = offset.into();
            changed["control_size"] = (pointer.size() - offset).into();
            let changed = serde_json::from_value(changed).unwrap();
            assert!(load_capsule_run_control(&layout, &changed).await.is_err());
            assert!(load_capsule_run(&layout, &changed).await.is_err());
        }
    }
}

#[tokio::test]
async fn run_pointer_admission_rejects_invalid_descriptors_before_io() {
    use std::sync::atomic::Ordering;
    let (layout, pointer, reads) = run_fixture().await;
    for field in ["capsule_count", "empty_control", "overflow"] {
        let mut changed = serde_json::to_value(&pointer).unwrap();
        match field {
            "capsule_count" => changed[field] = 0.into(),
            "empty_control" => {
                changed["control_offset"] = 0.into();
                changed["control_size"] = 0.into();
                changed["footer_hash"] = "".into();
            }
            _ => changed["control_offset"] = u64::MAX.into(),
        }
        let changed = serde_json::from_value(changed).unwrap();
        assert!(
            load_capsule_run(&layout, &changed).await.is_err(),
            "full read: {field}"
        );
        assert!(
            load_capsule_run_control(&layout, &changed).await.is_err(),
            "control read: {field}"
        );
        assert_eq!(reads.load(Ordering::SeqCst), 0, "{field}");
    }
}

#[tokio::test]
async fn pointer_catalog_resolves_dependencies_only_for_current_epoch_heads() {
    for ref_name in ["refs/heads/main", "refs/tags/release"] {
        for retired in [false, true] {
            let router = StoreLayout::new(
                Store::new(Arc::new(object_store::memory::InMemory::new())),
                "repositories/catalog-epoch".to_owned(),
            );
            let root = create_root(
                &router,
                RootRecord::encode(
                    RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            let transaction = CapsuleTransaction::new(
                root.record().digest(),
                vec![CapsuleRefEdit::new(
                    ref_name,
                    None,
                    Some("2".repeat(40)),
                    None,
                )],
            )
            .unwrap();
            let run =
                CapsuleRun::leaf(Capsule::build(&transaction, vec![], vec![]).unwrap()).unwrap();
            router
                .store()
                .create_strict_with_etag(&router.capsule_path(run.hash()), run.bytes().clone())
                .await
                .unwrap();
            let state = CapsuleRefState::from_checkpoint(
                "3".repeat(64),
                Some("2".repeat(40)),
                None,
                Some(transaction.id().unwrap()),
                vec![
                    crate::capsule_protocol::CapsulePointer::new(
                        run.hash(),
                        run.bytes().len() as u64,
                        run.control_offset(),
                        run.control_size(),
                        run.footer_hash(),
                        run.level(),
                        run.transaction_ids().to_vec(),
                        run.newest_base_root_digest(),
                    )
                    .unwrap(),
                ],
            )
            .unwrap();
            let epoch = if retired {
                "4".repeat(64)
            } else {
                "1".repeat(64)
            };
            let head = CapsuleRefHead::from_root(ref_name, epoch, None, None).unwrap();
            let activation_id = "5".repeat(64);
            let atomic = ref_name.starts_with("refs/tags/");
            let head = if atomic {
                head.prepare(
                    head.visible(&BTreeSet::new()).clone(),
                    activation_id.clone(),
                    state,
                )
                .unwrap()
            } else {
                head.commit(state).unwrap()
            };
            router
                .store()
                .create_strict_with_etag(
                    &router.capsule_ref_head_path(&capsule_ref_name_key(ref_name)),
                    head.encode().unwrap(),
                )
                .await
                .unwrap();

            let result = load_pointer_catalog(&router).await;
            if retired {
                assert_eq!(result.unwrap(), PointerCatalog::new(), "{ref_name}");
            } else if atomic {
                assert!(matches!(
                    result,
                    Err(MetadataError::Storage {
                        source: crab_storage::StorageError::NotFound { path }
                    }) if path == router.capsule_transaction_path(&activation_id).to_string()
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(MetadataError::CapsuleContract { reason, .. })
                        if reason.contains("checkpoint absent from the repository root")
                ));
            }
        }
    }
}

#[tokio::test]
async fn git_control_keeps_oid_admission_cold_and_paged_queries_obey_budgets() {
    use crate::capsule_protocol::{CapsuleGitPack, CapsulePointer};
    use std::sync::atomic::{AtomicU64, Ordering};
    let bytes = Arc::new(AtomicU64::new(0));
    let observed = bytes.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_byte_observer(Arc::new(move |read| {
            observed.fetch_add(read, Ordering::Relaxed);
        }));
    let layout = StoreLayout::new(store, "repositories/lazy-admission".into());
    let transaction = CapsuleTransaction::new(
        &"1".repeat(64),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("2".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let pack = CapsuleGitPack::new(
        bytes::Bytes::from_static(b"PACK"),
        bytes::Bytes::from_static(b"index"),
        bytes::Bytes::from_static(b"reverse"),
        bytes::Bytes::from_static(b"locator"),
        "4".repeat(40),
        10000,
    )
    .unwrap();
    let capsule = Capsule::build(&transaction, vec![pack], vec![]).unwrap();
    let oids = (0u32..10000)
        .map(|index| {
            let mut oid = [0; 20];
            oid[..4].copy_from_slice(&index.to_be_bytes());
            oid
        })
        .collect::<Vec<_>>();
    let run = CapsuleRun::leaf_with_member_oids(capsule, vec![oids.clone()]).unwrap();
    layout
        .store()
        .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
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
    bytes.store(0, Ordering::Relaxed);
    let (control, controls) = load_capsule_run_git_control(&layout, &pointer)
        .await
        .unwrap();
    let opening_bytes = bytes.swap(0, Ordering::Relaxed);
    assert!(opening_bytes < 16 * 1024 && control.admission().is_none());
    assert_eq!(controls.len(), 1);
    let reader = control
        .admission_reader(&layout, 128 * 1024)
        .unwrap()
        .unwrap();
    let requested = [oids[9999], oids[0], oids[9999], [0xff; 20]];
    let expected = vec![Some(vec![0]), Some(vec![0]), Some(vec![0]), None];
    assert_eq!(
        reader.lookup(layout.store(), &requested).await.unwrap(),
        expected
    );
    assert!(bytes.swap(0, Ordering::Relaxed) < 128 * 1024);
    assert_eq!(
        reader.lookup(layout.store(), &requested).await.unwrap(),
        expected
    );
    assert_eq!(bytes.swap(0, Ordering::Relaxed), 0);
    let limited = control.admission_reader(&layout, 1).unwrap().unwrap();
    assert!(limited.lookup(layout.store(), &requested).await.is_err());
    assert_eq!(bytes.load(Ordering::Relaxed), 0);
    // Unrequested OID corruption must remain cold during Git control opening;
    // exact lookup and complete inspection still reject the damaged dependency.
    let mut damaged = run.bytes().to_vec();
    damaged[run.control_offset() as usize] ^= 1;
    layout
        .store()
        .put_overwrite(&layout.capsule_path(run.hash()), damaged.into())
        .await
        .unwrap();
    let fresh = control
        .admission_reader(&layout, 128 * 1024)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            load_capsule_run_git_control(&layout, &pointer)
                .await
                .is_ok(),
            load_capsule_run_control(&layout, &pointer).await.is_err(),
            fresh.lookup(layout.store(), &[oids[0]]).await.is_err(),
        ),
        (true, true, true),
    );
}

#[tokio::test]
async fn visibility_lookup_keeps_oid_pages_cold_until_requested() {
    use crate::capsule_protocol::{
        CapsulePointer, CapsuleSection, CapsuleVisibilityDelta, VisibilityChange,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    let read_bytes = Arc::new(AtomicU64::new(0));
    let observed = read_bytes.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_byte_observer(Arc::new(move |bytes| {
            observed.fetch_add(bytes, Ordering::Relaxed);
        }));
    let layout = StoreLayout::new(store, "repositories/paged-visibility".into());
    let oids = (0u32..10000)
        .map(|index| {
            let mut oid = [0; 20];
            oid[..4].copy_from_slice(&index.to_be_bytes());
            oid
        })
        .collect::<Vec<_>>();
    let objects = oids
        .iter()
        .map(crate::git_visibility::encode_oid)
        .collect::<Vec<_>>();
    let tip = objects[7000].clone();
    let transaction = CapsuleTransaction::new(
        &"1".repeat(64),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some(tip.clone()),
            None,
        )],
    )
    .unwrap();
    let delta = CapsuleVisibilityDelta::new(BTreeMap::from([(
        "refs/heads/main".into(),
        crate::git_visibility::GitVisibilityEdit::from_replacement_objects(None, tip, objects),
    )]))
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        vec![],
        vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            delta.encode().unwrap(),
        )],
    )
    .unwrap();
    let run = CapsuleRun::leaf(capsule.clone()).unwrap();
    layout
        .store()
        .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
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
    let footer = layout
        .store()
        .range_get(
            &layout.capsule_path(run.hash()),
            pointer.control_offset()..pointer.size(),
        )
        .await
        .unwrap();
    let control = CapsuleRunControl::decode_footer(footer, &pointer).unwrap();
    assert!(read_bytes.swap(0, Ordering::Relaxed) < 4096);
    let reader = control
        .visibility_reader(&layout, capsule.hash(), 128 * 1024)
        .unwrap()
        .unwrap();
    let requested = [oids[9999], oids[0], oids[9999], [0xff; 20]];
    let expected = Some(vec![
        Some(VisibilityChange::Added),
        Some(VisibilityChange::Added),
        Some(VisibilityChange::Added),
        None,
    ]);
    assert_eq!(
        reader
            .lookup(layout.store(), "refs/heads/main", &requested)
            .await
            .unwrap(),
        expected
    );
    assert!(read_bytes.swap(0, Ordering::Relaxed) < 128 * 1024);
    assert_eq!(
        reader
            .lookup(layout.store(), "refs/heads/main", &requested)
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        reader
            .lookup(layout.store(), "refs/heads/missing", &requested)
            .await
            .unwrap(),
        None
    );
    assert_eq!(read_bytes.swap(0, Ordering::Relaxed), 0);
    let limited = control
        .visibility_reader(&layout, capsule.hash(), 1)
        .unwrap()
        .unwrap();
    assert!(
        limited
            .lookup(layout.store(), "refs/heads/main", &requested)
            .await
            .is_err()
    );
    assert_eq!(read_bytes.load(Ordering::Relaxed), 0);
    let mut damaged = run.bytes().to_vec();
    damaged[control.capsule_locations()[0]
        .visibility()
        .unwrap()
        .offset() as usize] ^= 1;
    layout
        .store()
        .put_overwrite(&layout.capsule_path(run.hash()), damaged.into())
        .await
        .unwrap();
    let fresh = control
        .visibility_reader(&layout, capsule.hash(), 128 * 1024)
        .unwrap()
        .unwrap();
    assert!(
        fresh
            .lookup(layout.store(), "refs/heads/main", &[oids[0]])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn ref_capture_rejects_uncommitted_base_changes_only_in_the_active_epoch() {
    for retired in [false, true] {
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "repositories/ref-capture".to_owned(),
        );
        let root = RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap();
        let name = "refs/tags/uncommitted";
        let head = CapsuleRefHead::from_root(
            name,
            if retired {
                "2".repeat(64)
            } else {
                root.ref_epoch().to_owned()
            },
            Some("3".repeat(40)),
            None,
        )
        .unwrap();
        layout
            .store()
            .create_strict(
                &layout.capsule_ref_head_path(&capsule_ref_name_key(name)),
                head.encode().unwrap(),
            )
            .await
            .unwrap();
        let result = capture_ref_heads(&layout, &root).await;
        if retired {
            assert!(result.unwrap().0.is_empty());
        } else {
            assert!(matches!(result, Err(MetadataError::CorruptObject { .. })));
        }
    }
}
