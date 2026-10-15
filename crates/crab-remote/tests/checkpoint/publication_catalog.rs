use super::*;
use crab_metadata::capsule_protocol::{
    FileCatalogEntry, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
};
use crab_remote::prepare::{ContentArtifact, ContentFile, PreparedContent};
use crab_xet::hash::MerkleHash;

async fn native_content(
    layout: &StoreLayout<Store>,
    directory: &std::path::Path,
) -> (PreparedContent, Vec<u8>, String) {
    let data = Bytes::from(vec![7; 8192]);
    let hash = *blake3::hash(&data).as_bytes();
    let chunk = crab_xet::xorb::format::Chunk::new(data.clone());
    let mut builder = crab_xet::xorb::builder::XorbBuilder::new();
    builder
        .push(&chunk, crab_xet::xorb::builder::RunId(0))
        .unwrap();
    let xorb = builder.finalize().unwrap().remove(0);
    let placements = xorb
        .placements
        .iter()
        .map(|entry| (entry.chunk_hash, entry.clone()))
        .collect();
    let file = crab_xet::shard::file_info_from_placements(
        MerkleHash::from(hash),
        &[chunk.hash],
        &placements,
    )
    .unwrap();
    let dependency =
        crab_xet::shard::xorb_info_from_placements(xorb.hash, &xorb.placements).unwrap();
    let mut shards = crab_xet::shard::PushShardSession::new();
    shards
        .add_file_bundle(file, &[Arc::new(dependency)])
        .unwrap();
    let (shard, shard_hash) = shards.finalize().unwrap().remove(0);
    let shard = Bytes::from(shard);
    let xorb_path = directory.join("xorb");
    let shard_path = directory.join("shard");
    std::fs::write(&xorb_path, &xorb.bytes).unwrap();
    std::fs::write(&shard_path, &shard).unwrap();
    layout
        .store()
        .put_overwrite(&layout.xorb_path(&xorb.hash), xorb.bytes.clone())
        .await
        .unwrap();
    layout
        .store()
        .put_overwrite(&layout.shard_path(&shard_hash), shard.clone())
        .await
        .unwrap();
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
        vec![ContentFile::new(hash, data.len() as u64, shard_hash.into())],
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let pointer = format!(
        "version https://crab.build/spec/v1\nfile-hash {}\nsize {}\n",
        blake3::Hash::from(hash).to_hex(),
        data.len()
    )
    .into_bytes();
    (content, pointer, MerkleHash::from(hash).hex())
}

async fn publish_history(layout: &StoreLayout<Store>, catalog: PointerCatalog) {
    let root = crab_write::capsule_protocol::open_root(layout)
        .await
        .unwrap();
    let (oid, pack) = blob_pack(b"history marker");
    let name = "refs/tags/history";
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(name, None, Some(oid.clone()), None)],
    )
    .unwrap();
    let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
        name.to_owned(),
        GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid]),
    )]))
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        vec![pack],
        vec![
            CapsuleSection::new(
                CapsuleSectionKind::CatalogDelta,
                catalog.encode_delta().unwrap(),
            ),
            CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            ),
        ],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(layout, root, &transaction, &capsule)
        .await
        .unwrap();
}

fn dependency_limits() -> crab_read::dependency_proof::DependencyProofLimits {
    crab_read::dependency_proof::DependencyProofLimits {
        max_dependencies: 16,
        max_total_file_bytes: LIMIT,
        max_duration: std::time::Duration::from_secs(30),
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
            max_duration: std::time::Duration::from_secs(30),
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fixed_pointer_publication_keeps_unrelated_catalog_history_cold() {
    for count in [100, 10_000] {
        for checkpointed in [false, true] {
            for supplied in [false, true] {
                let (layout, observations) = empty_fixture().await;
                let directory = tempfile::tempdir().unwrap();
                let (content, pointer, file_hash) = native_content(&layout, directory.path()).await;
                let mut catalog = PointerCatalog::new();
                for index in 0..count {
                    let shard = format!("{:064x}", index + 1_000_000);
                    let xorb = format!("{:064x}", index + 2_000_000);
                    catalog
                        .insert_xorb(
                            xorb.clone(),
                            XorbCatalogEntry::new(
                                99,
                                "e".repeat(64),
                                vec![XorbChunkEntry::new(
                                    format!("{:064x}", index + 3_000_000),
                                    7,
                                )],
                            ),
                        )
                        .unwrap();
                    catalog
                        .insert_shard(shard.clone(), ShardCatalogEntry::new(80, vec![xorb]))
                        .unwrap();
                    catalog
                        .insert_file(format!("{index:064x}"), FileCatalogEntry::new(7, shard))
                        .unwrap();
                }
                // A new file may reuse already-published shard/xorb identities.
                // Omitting its binding makes explicit artifact selection necessary.
                for (hash, entry) in content.pointer_catalog().xorbs() {
                    catalog.insert_xorb(hash.clone(), entry.clone()).unwrap();
                }
                for (hash, entry) in content.pointer_catalog().shards() {
                    catalog.insert_shard(hash.clone(), entry.clone()).unwrap();
                }
                if !supplied {
                    for (hash, entry) in content.pointer_catalog().files() {
                        catalog.insert_file(hash.clone(), entry.clone()).unwrap();
                    }
                }
                publish_history(&layout, catalog.clone()).await;
                if checkpointed {
                    assert!(
                        crab_remote::checkpoint::publish_capsule_checkpoint(
                            &layout,
                            0,
                            64 * LIMIT,
                            &CancellationToken::new(),
                        )
                        .await
                        .unwrap()
                        .published
                    );
                }
                let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
                let cancel = CancellationToken::new();
                let target = "refs/tags/pointer".to_owned();
                let lease_store = layout.store().clone();
                let lease_layout = layout.clone();
                let reader_runtime = runtime.clone();
                crab_remote::publication::with_leases(
                    &lease_store, &lease_layout, [target.clone()], std::time::Duration::from_secs(60), &cancel,
                    move |_holders, cancel| async move {
                        let view = open_history_view(&layout).await;
                        assert_eq!(view.pointer_catalog().await?, catalog);
                        let repository = view.git_repository_from_store(
                            layout.clone(),
                            crab_remote_git::RepositoryIdentity::new("memory", "selected-publication", 1).unwrap(),
                            reader_runtime, crab_remote_git::RepositoryOptions::default(), LIMIT, &cancel,
                        ).await?;
                        let (oid, pack) = blob_pack(&pointer);
                        let path = directory.path().join("incoming.pack");
                        std::fs::write(&path, pack.pack_bytes())?;
                        let mut prepared = crab_remote::prepare::prepare_capsule(
                            repository, view.git_visibility_index().await?,
                            crab_git::incoming_pack::ScratchSpace::new(directory.path(), 64 * LIMIT),
                            Some(std::io::BufReader::new(std::fs::File::open(path)?)),
                            vec![crab_git::receive_plan::RefUpdate { name: target.clone(), old: None, new: Some(gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap()) }],
                            BTreeMap::new(), &cancel,
                            crab_remote::prepare::Options {
                                layout: layout.clone(),
                                graph: crab_git::receive_plan::GraphLimits { max_ref_updates: 16, max_graph_steps: 1024, max_object_bytes: LIMIT as usize, max_read_bytes: LIMIT },
                                pack: crab_git::incoming_pack::ReceiveLimits { max_pack_bytes: LIMIT, max_objects: 1024, max_object_bytes: LIMIT as usize, max_inflated_bytes: LIMIT, max_delta_depth: 128 },
                                policy: |_: &str| crab_git::receive_plan::RefPolicy { allow_delete: true, allow_non_fast_forward: false },
                            },
                        ).await?;
                        assert_eq!(prepared.plan().pointers().len(), 1);
                        if supplied { prepared = prepared.attach_content(content)?; }
                        observations.0.lock().unwrap().clear();
                        let artifacts = prepared.upload_capsule(&view, dependency_limits(), &cancel).await?;
                        let requests = observations.0.lock().unwrap().clone();
                        assert!(requests.iter().all(|read| read.operation != StorageOperation::Put), "reused dependencies must not upload payloads");
                        let reads = requests.iter().filter(|r| matches!(r.operation, StorageOperation::Get | StorageOperation::Range)).collect::<Vec<_>>();
                        assert!(reads.len() <= 24 && reads.iter().map(|r| r.bytes_read).sum::<u64>() <= 512 * 1024, "fixed publication exceeds selected dependency bounds: count={count}, checkpoint={checkpointed}, supplied={supplied}");
                        let outcome = artifacts.commit(None, std::time::Duration::from_secs(60), &cancel).await?;
                        assert!(matches!(outcome, crab_remote::prepare::CapsuleCommitOutcome::Committed { .. }));
                        let after = open_history_view(&layout).await;
                        assert_eq!(after.refs()[&target], oid);
                        assert!(after.pointer_catalog_lookup(&layout, LIMIT)?.files(layout.store(), &[file_hash.clone()]).await?.files().contains_key(&file_hash));
                        drop(prepared);
                        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
                    },
                ).await.unwrap();
                runtime.shutdown().await;
            }
        }
    }
}

async fn open_history_view(layout: &StoreLayout<Store>) -> CapsuleRepositoryView {
    let root = crab_write::capsule_protocol::open_root(layout)
        .await
        .unwrap();
    crab_read::capsule_protocol::open_git_view_from_root(
        layout,
        root,
        CapsuleReadLimits {
            max_capsule_bytes: 64 * LIMIT,
            max_frontier_bytes: 64 * LIMIT,
        },
    )
    .await
    .unwrap()
}
