#![expect(clippy::unwrap_used, reason = "test assertions")]

use super::*;
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
    catalog
        .insert_file(
            MerkleHash::from(pointer.file_hash).hex(),
            FileCatalogEntry::new(pointer.size, shard_hash.hex()),
        )
        .unwrap();
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
    let proof = verify_installed_dependencies(
        &layout,
        &workspace.path().join("repository.git"),
        &refs,
        &catalog,
        SCAN_LIMITS,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(proof.reachable_crab_pointers, 1);
}

#[tokio::test]
async fn installed_dependency_proof_rejects_a_valid_catalog_with_wrong_file_hash() {
    let (layout, catalog, workspace, refs) = fixture("wrong_hash").await;
    crate::verify_capsule_pointer_catalog_objects(&layout, &catalog)
        .await
        .unwrap();
    let result = verify_installed_dependencies(
        &layout,
        &workspace.path().join("repository.git"),
        &refs,
        &catalog,
        SCAN_LIMITS,
        &CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(result, Err(ReadError::HashMismatch { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn installed_dependency_proof_rejects_missing_short_or_reordered_recipes() {
    for case in ["missing_recipe", "short_recipe", "wrong_order"] {
        let (layout, catalog, workspace, refs) = fixture(case).await;
        crate::verify_capsule_pointer_catalog_objects(&layout, &catalog)
            .await
            .unwrap();
        let result = verify_installed_dependencies(
            &layout,
            &workspace.path().join("repository.git"),
            &refs,
            &catalog,
            SCAN_LIMITS,
            &CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(ReadError::HashMismatch { .. } | ReadError::CorruptObject { .. })
            ),
            "{case}: {result:?}",
        );
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
