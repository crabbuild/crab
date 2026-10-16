#![expect(clippy::unwrap_used, reason = "test assertions")]

use super::super::*;
use super::{DependencyCatalog, verify_installed_dependencies_from};
use crab_metadata::capsule_protocol::{
    FileCatalogEntry, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
};
use crab_types::pointer::Pointer;
use crab_xet::{
    hash::MerkleHash,
    shard::{ShardWriter, file_info_from_placements, xorb_info_from_placements},
    xorb::{
        builder::{RunId, XorbBuilder},
        format::Chunk,
    },
};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const SCAN_LIMITS: crab_git::walk::PointerScanLimits = crab_git::walk::PointerScanLimits {
    objects: 16,
    lookups: 64,
    allocation_bytes: 1024 * 1024,
};

async fn fixture(
    recipe_case: &str,
) -> (
    StoreLayout<Store>,
    PointerCatalog,
    tempfile::TempDir,
    BTreeMap<String, String>,
) {
    let layout = StoreLayout::new(
        Store::new(Arc::new(object_store::memory::InMemory::new())),
        "recipe-proof".to_owned(),
    );
    let chunks =
        [b"alpha".as_slice(), b"beta"].map(|body| Chunk::new(Bytes::copy_from_slice(body)));
    let mut builder = XorbBuilder::new();
    for chunk in &chunks {
        builder.push(chunk, RunId(0)).unwrap();
    }
    let xorb = builder.finalize().unwrap().pop().unwrap();
    let pointer = Pointer {
        file_hash: if recipe_case == "wrong_hash" {
            [42; 32]
        } else {
            blake3::hash(b"betaalphabeta").into()
        },
        size: 13,
        shard_hint: None,
    };
    // The repeated, reordered recipe is intentional: a set of valid chunks is
    // not proof of the ordered bytes promised by the file hash.
    let mut recipe = file_info_from_placements(
        MerkleHash::from(pointer.file_hash),
        &[chunks[1].hash, chunks[0].hash, chunks[1].hash],
        &xorb
            .placements
            .iter()
            .map(|p| (p.chunk_hash, p.clone()))
            .collect(),
    )
    .unwrap();
    match recipe_case {
        "wrong_order" => recipe.segments.reverse(),
        "short_recipe" => {
            recipe.segments.pop();
            recipe.metadata.num_entries = recipe.segments.len() as u32;
        }
        _ => {}
    }
    let mut shard = ShardWriter::new();
    shard
        .add_xorb(Arc::new(
            xorb_info_from_placements(xorb.hash, &xorb.placements).unwrap(),
        ))
        .unwrap();
    if recipe_case != "missing_recipe" {
        shard.add_file(recipe).unwrap();
    }
    let (shard_body, shard_hash) = shard.finalize().unwrap();
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_xorb(
            xorb.hash.hex(),
            XorbCatalogEntry::new(
                xorb.bytes.len() as u64,
                blake3::hash(&xorb.bytes).to_hex().to_string(),
                chunks
                    .iter()
                    .map(|chunk| XorbChunkEntry::new(chunk.hash.hex(), chunk.data.len() as u32))
                    .collect(),
            ),
        )
        .unwrap();
    catalog
        .insert_shard(
            shard_hash.hex(),
            ShardCatalogEntry::new(shard_body.len() as u64, vec![xorb.hash.hex()]),
        )
        .unwrap();
    if recipe_case != "missing_binding" {
        catalog
            .insert_file(
                MerkleHash::from(pointer.file_hash).hex(),
                FileCatalogEntry::new(pointer.size, shard_hash.hex()),
            )
            .unwrap();
    }
    layout
        .store()
        .put(&layout.xorb_path(&xorb.hash), xorb.bytes)
        .await
        .unwrap();
    layout
        .store()
        .put(&layout.shard_path(&shard_hash), Bytes::from(shard_body))
        .await
        .unwrap();

    let workspace = tempfile::tempdir().unwrap();
    let git_dir = workspace.path().join("repository.git");
    crab_git::initialize_bare_git_dir(&git_dir).unwrap();
    let oid = write_pointer(workspace.path(), &pointer);
    (
        layout,
        catalog,
        workspace,
        BTreeMap::from([("refs/tags/file".to_owned(), oid)]),
    )
}

async fn captured_catalog(
    layout: &StoreLayout<Store>,
    catalog: &PointerCatalog,
    refs: &BTreeMap<String, String>,
) -> CapsuleRepositoryView {
    use crab_metadata::capsule_protocol::{
        CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction, RepositoryRoot,
    };
    let initial =
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap();
    let transaction = CapsuleTransaction::new(
        initial.digest(),
        refs.iter()
            .map(|(name, oid)| CapsuleRefEdit::new(name, None, Some(oid.clone()), None))
            .collect(),
    )
    .unwrap();
    let run = CapsuleRun::leaf(
        Capsule::build(
            &transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::CatalogDelta,
                catalog.encode_delta().unwrap(),
            )],
        )
        .unwrap(),
    )
    .unwrap();
    layout
        .store()
        .put(&layout.capsule_path(run.hash()), run.bytes().clone())
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
    let root = initial
        .root()
        .advance(
            initial.digest(),
            refs.clone(),
            BTreeMap::new(),
            vec![pointer],
            &transaction.id().unwrap(),
        )
        .unwrap();
    let root = RootRecord::encode(root).unwrap();
    layout
        .store()
        .put_overwrite(&layout.capsule_root_path(), root.bytes().clone())
        .await
        .unwrap();
    open_view(
        layout,
        CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        },
    )
    .await
    .unwrap()
}

async fn verify_fixture(
    layout: &StoreLayout<Store>,
    workspace: &Path,
    refs: &BTreeMap<String, String>,
    catalog: &PointerCatalog,
    captured: bool,
) -> Result<CapsuleDependencyProof> {
    let cancel = CancellationToken::new();
    let git_dir = workspace.join("repository.git");
    if !captured {
        return verify_installed_dependencies(
            layout,
            &git_dir,
            refs,
            catalog,
            SCAN_LIMITS,
            &cancel,
        )
        .await;
    }
    let view = captured_catalog(layout, catalog, refs).await;
    let lookup = view
        .pointer_catalog_lookup(layout, 16 * 1024 * 1024)
        .unwrap();
    verify_installed_dependencies_from(
        layout,
        &git_dir,
        refs,
        DependencyCatalog::Captured {
            view: &view,
            lookup: &lookup,
        },
        SCAN_LIMITS,
        &cancel,
    )
    .await
}

fn write_pointer(workspace: &Path, pointer: &Pointer) -> String {
    let file = workspace.join("pointer");
    std::fs::write(&file, pointer.serialize()).unwrap();
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(workspace.join("repository.git"))
        .args(["hash-object", "-w"])
        .arg(file)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn installed_dependency_proof_accepts_reordered_repeated_file_chunks() {
    let (layout, catalog, workspace, refs) = fixture("valid").await;
    for captured in [false, true] {
        let proof = verify_fixture(&layout, workspace.path(), &refs, &catalog, captured)
            .await
            .unwrap();
        assert_eq!(
            proof,
            CapsuleDependencyProof {
                catalog_files: 1,
                catalog_shards: 1,
                catalog_xorbs: 1,
                catalog_objects_read: 2,
                reachable_crab_pointers: 1,
                reachable_lfs_objects: 0,
                reachable_lfs_bytes: 0
            }
        );
    }
}

#[tokio::test]
async fn installed_dependency_proof_rejects_a_valid_catalog_with_wrong_file_hash() {
    let (layout, catalog, workspace, refs) = fixture("wrong_hash").await;
    crate::verify_capsule_pointer_catalog_objects(&layout, &catalog)
        .await
        .unwrap();
    for captured in [false, true] {
        let result = verify_fixture(&layout, workspace.path(), &refs, &catalog, captured).await;
        assert!(
            matches!(result, Err(ReadError::HashMismatch { .. })),
            "{captured}: {result:?}"
        );
    }
}

#[tokio::test]
async fn installed_dependency_proof_rejects_missing_short_or_reordered_recipes() {
    for case in ["missing_recipe", "short_recipe", "wrong_order"] {
        let (layout, catalog, workspace, refs) = fixture(case).await;
        crate::verify_capsule_pointer_catalog_objects(&layout, &catalog)
            .await
            .unwrap();
        for captured in [false, true] {
            let result = verify_fixture(&layout, workspace.path(), &refs, &catalog, captured).await;
            assert!(
                matches!(
                    result,
                    Err(ReadError::HashMismatch { .. } | ReadError::CorruptObject { .. })
                ),
                "{case}/{captured}: {result:?}"
            );
        }
    }
}

#[derive(Default)]
struct ReadCount(AtomicUsize);

impl crab_storage::StorageObserver for ReadCount {
    fn started(&self, _: crab_storage::StorageOperation) {}

    fn finished(&self, observation: crab_storage::StorageObservation) {
        if observation.operation == crab_storage::StorageOperation::Get {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[tokio::test]
async fn distinct_pointer_hints_share_one_file_proof_but_not_size_validation() {
    let (layout, catalog, workspace, mut refs) = fixture("valid").await;
    let reads = Arc::new(ReadCount::default());
    let layout = StoreLayout::new(
        layout.store().clone().with_storage_observer(reads.clone()),
        layout.repo_prefix().to_owned(),
    );
    let git_dir = workspace.path().join("repository.git");
    let cancel = CancellationToken::new();
    verify_installed_dependencies(&layout, &git_dir, &refs, &catalog, SCAN_LIMITS, &cancel)
        .await
        .unwrap();
    let baseline = reads.0.swap(0, Ordering::Relaxed);
    assert!(baseline > 0);
    let mut pointer =
        Pointer::parse(&std::fs::read(workspace.path().join("pointer")).unwrap()).unwrap();
    pointer.shard_hint = Some([7; 32]);
    refs.insert(
        "refs/tags/hinted".to_owned(),
        write_pointer(workspace.path(), &pointer),
    );
    let proof =
        verify_installed_dependencies(&layout, &git_dir, &refs, &catalog, SCAN_LIMITS, &cancel)
            .await
            .unwrap();
    assert_eq!(proof.reachable_crab_pointers, 2);
    assert_eq!(reads.0.load(Ordering::Relaxed), baseline);

    pointer.size += 1;
    refs.insert(
        "refs/tags/wrong-size".to_owned(),
        write_pointer(workspace.path(), &pointer),
    );
    assert!(matches!(
        verify_installed_dependencies(&layout, &git_dir, &refs, &catalog, SCAN_LIMITS, &cancel)
            .await,
        Err(ReadError::CorruptObject { .. }),
    ));
}

#[tokio::test]
async fn catalog_file_proof_preserves_missing_origin_errors() {
    for kind in ["shard", "xorb"] {
        let (layout, catalog, workspace, _) = fixture("valid").await;
        let pointer =
            Pointer::parse(&std::fs::read(workspace.path().join("pointer")).unwrap()).unwrap();
        let entry = catalog.files().values().next().unwrap();
        let path = if kind == "shard" {
            layout.shard_path(&MerkleHash::from_hex(entry.shard_hash()).unwrap())
        } else {
            layout.xorb_path(&MerkleHash::from_hex(catalog.xorbs().keys().next().unwrap()).unwrap())
        };
        layout.store().delete(&path).await.unwrap();
        assert!(
            matches!(
                crate::verify_catalog_file_recipe(
                    &layout,
                    &pointer,
                    entry,
                    &CancellationToken::new()
                )
                .await,
                Err(ReadError::Storage(
                    crab_storage::StorageError::NotFound { .. }
                )),
            ),
            "{kind}"
        );
    }
}

#[tokio::test]
async fn catalog_file_proof_cancellation_interrupts_pending_shard_read() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    use std::time::Duration;
    let (layout, catalog, workspace, _) = fixture("valid").await;
    let pointer =
        Pointer::parse(&std::fs::read(workspace.path().join("pointer")).unwrap()).unwrap();
    let entry = catalog.files().values().next().unwrap();
    let layout = StoreLayout::new(
        Store::new(Arc::new(ThrottledStore::new(
            Arc::clone(layout.store().inner()),
            ThrottleConfig {
                wait_get_per_call: Duration::from_secs(10),
                ..Default::default()
            },
        ))),
        layout.repo_prefix().to_owned(),
    );
    let cancel = CancellationToken::new();
    let proof = crate::verify_catalog_file_recipe(&layout, &pointer, entry, &cancel);
    tokio::pin!(proof);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut proof)
            .await
            .is_err()
    );
    cancel.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), &mut proof)
            .await
            .unwrap(),
        Err(ReadError::Cancelled),
    ));
}

#[tokio::test]
async fn git_view_catalog_materialization_uses_the_captured_checkpoint_after_root_changes() {
    use crab_metadata::capsule_protocol::{
        CapsuleGitPack, CapsuleRefEdit, CapsuleTransaction, HistorySegment, HistorySegmentState,
        LayeredVisibilitySnapshot, PackLayer, RepositoryRoot, source_catalog_digest,
    };
    use crab_metadata::git_visibility::GitVisibilityIndex;

    let (layout, catalog, _workspace, refs) = fixture("valid").await;
    let initial =
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap();
    let pack = CapsuleGitPack::new(
        Bytes::from_static(b"pack"),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "2".repeat(40),
        1,
    )
    .unwrap();
    let transaction = CapsuleTransaction::new(
        initial.digest(),
        vec![CapsuleRefEdit::new(
            "refs/tags/file",
            None,
            refs.get("refs/tags/file").cloned(),
            None,
        )],
    )
    .unwrap();
    let run =
        CapsuleRun::leaf(Capsule::build(&transaction, vec![pack.clone()], Vec::new()).unwrap())
            .unwrap();
    let run_pointer = CapsulePointer::new(
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
    let initial = RootRecord::encode(
        initial
            .root()
            .advance(
                initial.digest(),
                refs.clone(),
                BTreeMap::new(),
                vec![run_pointer],
                &transaction.id().unwrap(),
            )
            .unwrap(),
    )
    .unwrap();
    let layer = PackLayer::build(&pack).unwrap();
    let source = layer.source_descriptor().unwrap();
    let visibility = LayeredVisibilitySnapshot::from_index(
        &GitVisibilityIndex::new(
            1,
            "0".repeat(64),
            "0".repeat(64),
            refs.into_iter()
                .map(|(name, oid)| (name, vec![oid]))
                .collect(),
        )
        .unwrap(),
        &source_catalog_digest(std::slice::from_ref(&source)).unwrap(),
    )
    .unwrap();
    let checkpoint = LayeredCheckpoint::build(
        initial.root().generation(),
        initial.digest(),
        vec![source],
        catalog.clone(),
        Some(visibility),
    )
    .unwrap();
    let pointer = CheckpointPointer::new_layered(
        checkpoint.hash(),
        checkpoint.bytes().len() as u64,
        checkpoint.control_offset(),
        checkpoint.control_size(),
        checkpoint.footer_hash(),
        checkpoint.covered_generation(),
        checkpoint.covered_root_digest(),
        checkpoint.pack_count().unwrap(),
        checkpoint.object_count().unwrap(),
    )
    .unwrap();
    let history = HistorySegment::build(
        pointer.clone(),
        None,
        HistorySegmentState::new(
            initial.root().refs().clone(),
            initial.root().peeled_refs().clone(),
            initial.root().head().to_owned(),
            initial.root().compacted_ref_transactions().clone(),
            initial.root().capsule_frontier().to_vec(),
        ),
    )
    .unwrap();
    let root = RootRecord::encode(
        initial
            .root()
            .install_checkpoint(initial.digest(), pointer, Some(history.pointer().unwrap()))
            .unwrap(),
    )
    .unwrap();
    checkpoint.publish_catalog(&layout).await.unwrap();
    layout
        .store()
        .put(
            &layout.capsule_checkpoint_path(checkpoint.hash()),
            checkpoint.bytes().clone(),
        )
        .await
        .unwrap();
    layout
        .store()
        .put(&layout.capsule_root_path(), root.bytes().clone())
        .await
        .unwrap();
    let view = open_git_view_from_root(
        &layout,
        load_root(&layout).await.unwrap(),
        CapsuleReadLimits {
            max_capsule_bytes: 1024 * 1024,
            max_frontier_bytes: 1024 * 1024,
        },
    )
    .await
    .unwrap();
    assert!(view.layered_checkpoint().unwrap().is_control_only());
    // The bulk read must follow the captured pointer rather than recapturing a
    // mutable root; otherwise hydration could silently select a different recipe.
    layout
        .store()
        .put_overwrite(&layout.capsule_root_path(), initial.bytes().clone())
        .await
        .unwrap();
    assert_eq!(view.pointer_catalog().await.unwrap(), catalog);
}

#[tokio::test]
async fn dependency_proof_rejects_missing_bindings_on_both_sources() {
    let (layout, catalog, workspace, refs) = fixture("missing_binding").await;
    for captured in [false, true] {
        let result = verify_fixture(&layout, workspace.path(), &refs, &catalog, captured).await;
        assert!(
            matches!(result, Err(ReadError::CorruptObject { reason, .. }) if reason.contains("absent from the catalog"))
        );
    }
}

#[tokio::test]
async fn captured_dependency_proof_keeps_pinned_bindings_after_root_replacement() {
    use crab_metadata::capsule_protocol::RepositoryRoot;
    let (layout, catalog, workspace, refs) = fixture("valid").await;
    let view = captured_catalog(&layout, &catalog, &refs).await;
    let lookup = view
        .pointer_catalog_lookup(&layout, 16 * 1024 * 1024)
        .unwrap();
    let replacement =
        RootRecord::encode(RepositoryRoot::initial(&"2".repeat(64), "refs/heads/main").unwrap())
            .unwrap();
    layout
        .store()
        .put_overwrite(&layout.capsule_root_path(), replacement.bytes().clone())
        .await
        .unwrap();
    let proof = verify_installed_dependencies_from(
        &layout,
        &workspace.path().join("repository.git"),
        &refs,
        DependencyCatalog::Captured {
            view: &view,
            lookup: &lookup,
        },
        SCAN_LIMITS,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(proof.reachable_crab_pointers, 1);
}

#[tokio::test]
async fn captured_dependency_proof_rejects_unrelated_incomplete_catalog_dependencies() {
    let (layout, mut catalog, workspace, refs) = fixture("valid").await;
    catalog
        .insert_file("a".repeat(64), FileCatalogEntry::new(7, "b".repeat(64)))
        .unwrap();
    let result = verify_fixture(&layout, workspace.path(), &refs, &catalog, true).await;
    assert!(matches!(
        result,
        Err(ReadError::Metadata(
            crab_metadata::error::MetadataError::CorruptObject { .. }
        ))
    ));
}

#[tokio::test]
async fn captured_binding_batches_do_not_repeat_file_reconstruction() {
    let (layout, catalog, workspace, mut refs) = fixture("valid").await;
    let reads = Arc::new(ReadCount::default());
    let layout = StoreLayout::new(
        layout.store().clone().with_storage_observer(reads.clone()),
        layout.repo_prefix().to_owned(),
    );
    let git_dir = workspace.path().join("repository.git");
    verify_installed_dependencies(
        &layout,
        &git_dir,
        &refs,
        &catalog,
        SCAN_LIMITS,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let baseline = reads.0.swap(0, Ordering::Relaxed);
    let mut pointer =
        Pointer::parse(&std::fs::read(workspace.path().join("pointer")).unwrap()).unwrap();
    for hint in 1..=65u8 {
        pointer.shard_hint = Some([hint; 32]);
        refs.insert(
            format!("refs/tags/hint-{hint}"),
            write_pointer(workspace.path(), &pointer),
        );
    }
    let view = captured_catalog(&layout, &catalog, &refs).await;
    let lookup = view
        .pointer_catalog_lookup(&layout, 16 * 1024 * 1024)
        .unwrap();
    reads.0.store(0, Ordering::Relaxed);
    let proof = verify_installed_dependencies_from(
        &layout,
        &git_dir,
        &refs,
        DependencyCatalog::Captured {
            view: &view,
            lookup: &lookup,
        },
        crab_git::walk::PointerScanLimits {
            objects: 256,
            lookups: 512,
            allocation_bytes: 1024 * 1024,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            proof.reachable_crab_pointers,
            reads.0.load(Ordering::Relaxed)
        ),
        (66, baseline)
    );
}

#[tokio::test]
async fn pointer_byte_phase_checks_selected_closures_without_claiming_global_catalog_health() {
    let (layout, mut catalog, workspace, refs) = fixture("valid").await;
    catalog
        .insert_file("a".repeat(64), FileCatalogEntry::new(7, "b".repeat(64)))
        .unwrap();
    let view = captured_catalog(&layout, &catalog, &refs).await;
    verify_captured_installed_pointer_bytes(
        &layout,
        &workspace.path().join("repository.git"),
        &view,
        SCAN_LIMITS,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(
        verify_fixture(&layout, workspace.path(), &refs, &catalog, true).await,
        Err(ReadError::Metadata(_))
    ));
}
