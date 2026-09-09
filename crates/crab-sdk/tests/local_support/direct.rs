#![cfg(feature = "local")]

use std::path::Path;

use crate::local_support::{commit, git_path, run};

pub async fn direct_remote(root: &Path, prefix: &str, commit_count: usize) -> Vec<String> {
    use bytes::Bytes;
    use crab_metadata::manifests::{
        BulkData, Manifest, PackManifestEntry, compact_pack_index, compact_shard_index,
    };
    use crab_storage::{Store, StoreLayout};
    use std::sync::Arc;

    let source = tempfile::tempdir().unwrap();
    run(
        &git_path(),
        source.path(),
        &["init", "--initial-branch=main"],
    );
    let mut commits = Vec::new();
    for index in 0..commit_count {
        commits.push(commit(
            &git_path(),
            source.path(),
            &format!("{index}.txt"),
            &format!("commit {index}"),
        ));
    }
    run(&git_path(), source.path(), &["tag", "retained"]);
    if commit_count > 1 {
        run(&git_path(), source.path(), &["tag", "removed", "HEAD~1"]);
    }
    let base = source.path().join("fixture");
    let git_sha = run(
        &git_path(),
        source.path(),
        &[
            "pack-objects",
            "--all",
            "--index-version=2",
            base.to_str().unwrap(),
        ],
    );
    let pack = source.path().join(format!("fixture-{git_sha}.pack"));
    let index = pack.with_extension("idx");
    let reverse = pack.with_extension("rev");
    crab_git::write_pack_reverse_index(&index, &reverse).unwrap();
    let pack_bytes = std::fs::read(&pack).unwrap();
    let pack_id = blake3::hash(&pack_bytes).to_hex().to_string();
    let object_count = crab_git::PackLocationIter::open(&index, &reverse, pack_bytes.len() as u64)
        .unwrap()
        .count() as u64;
    std::fs::create_dir_all(root).unwrap();
    let store = Store::new(Arc::new(
        object_store::local::LocalFileSystem::new_with_prefix(root).unwrap(),
    ));
    let layout = StoreLayout::new(store.clone(), prefix.to_owned());
    crab_metadata::layout_descriptor::ensure_canonical_layout(&store, &layout)
        .await
        .unwrap();
    for (path, bytes) in [
        (layout.pack_path(&pack_id), pack_bytes),
        (
            layout.pack_index_path(&pack_id),
            std::fs::read(index).unwrap(),
        ),
        (
            layout.pack_reverse_index_path(&pack_id),
            std::fs::read(reverse).unwrap(),
        ),
    ] {
        store.put(&path, Bytes::from(bytes)).await.unwrap();
    }
    let entry = PackManifestEntry {
        pack_id: pack_id.clone(),
        content_hash: pack_id,
        size: std::fs::metadata(pack).unwrap().len(),
        object_count,
        ref_tips: vec![commits.last().unwrap().clone()],
    };
    let (pack_index_hash, _, pack_index) = compact_pack_index(1, &[entry]).unwrap();
    let (shard_index_hash, _, shard_index) = compact_shard_index(1, &[]).unwrap();
    crab_metadata::manifest_store::upload_segmented_bulk(
        &store,
        &layout,
        &BulkData {
            pack_index,
            shard_index,
        },
    )
    .await
    .unwrap();
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    manifest
        .refs
        .insert("refs/heads/main".into(), commits.last().unwrap().clone());
    manifest
        .refs
        .insert("refs/tags/retained".into(), commits.last().unwrap().clone());
    if commit_count > 1 {
        manifest.refs.insert(
            "refs/tags/removed".into(),
            commits[commit_count - 2].clone(),
        );
    }
    manifest.pack_index_hash = pack_index_hash;
    manifest.shard_index_hash = shard_index_hash;
    manifest.seal_git_validation();
    crab_metadata::manifest_store::create_manifest(&store, &layout, &manifest)
        .await
        .unwrap();
    commits
}
