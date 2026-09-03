use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use crab_metadata::receipts::{
    CommittedChunkReceipt, OriginReceipt, RECEIPT_SCHEMA_VERSION, generation_file_index_digest,
};
use crab_metadata::remote_index::{RemoteIndexConfig, RemoteIndexWriter};
use crab_metadata::value_codec::CommittedFileRecord;
use crab_staging::shard_replay::{REPLAY_BATCH_ENTRIES, ShardReplaySpool};
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
use crab_xet::reconstruction::{ChunkPlacementMap, build_file_terms};
use crab_xet::shard::{
    FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, PushShardSession,
    XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};
use crab_xet::xorb::builder::XorbResult;
use crab_xet::xorb::format::ChunkPlacement;

use super::repack::{RepackedFile, ViewCrabObjects};
use crate::error::{AuthServerError, Result};

struct ViewShardPlan {
    shards: Vec<(Vec<u8>, MerkleHash)>,
}

pub(super) struct UploadedViewCrabObjects {
    pub shard_hashes: Vec<String>,
    shards: Vec<(Vec<u8>, MerkleHash)>,
    placement: ChunkPlacementMap,
    payload_digests: HashMap<MerkleHash, [u8; 32]>,
}

pub(super) async fn upload_view_crab_objects(
    store: &Store,
    router: &StoreLayout<Store>,
    objects: ViewCrabObjects,
) -> Result<UploadedViewCrabObjects> {
    let placement = placement_map(&objects.xorbs);
    let plan = build_view_shards(&objects.files, &objects.xorbs, &placement)?;

    for xorb in &objects.xorbs {
        store
            .put(&router.xorb_path(&xorb.hash), xorb.bytes.clone())
            .await?;
    }
    for (bytes, hash) in &plan.shards {
        store
            .put(&router.shard_path(hash), Bytes::from(bytes.clone()))
            .await?;
    }

    Ok(UploadedViewCrabObjects {
        shard_hashes: plan.shards.iter().map(|(_, hash)| hash.hex()).collect(),
        shards: plan.shards,
        placement,
        payload_digests: objects
            .xorbs
            .iter()
            .map(|xorb| (xorb.hash, xorb.payload_digest))
            .collect(),
    })
}

fn placement_map(xorbs: &[XorbResult]) -> ChunkPlacementMap {
    let mut placement = ChunkPlacementMap::new();
    for xorb in xorbs {
        for entry in &xorb.placements {
            placement.insert(
                entry.chunk_hash,
                ChunkPlacement {
                    chunk_hash: entry.chunk_hash,
                    xorb_hash: entry.xorb_hash,
                    chunk_index: entry.chunk_index,
                    uncompressed_size: entry.uncompressed_size,
                },
            );
        }
    }
    placement
}

fn build_view_shards(
    files: &[RepackedFile],
    xorbs: &[XorbResult],
    placement: &ChunkPlacementMap,
) -> Result<ViewShardPlan> {
    build_view_shards_with_session(files, xorbs, placement, PushShardSession::new())
}

fn build_view_shards_with_session(
    files: &[RepackedFile],
    xorbs: &[XorbResult],
    placement: &ChunkPlacementMap,
    mut shard_session: PushShardSession,
) -> Result<ViewShardPlan> {
    let mut xorb_info_by_hash = HashMap::with_capacity(xorbs.len());
    for xorb in xorbs {
        let mut chunks = xorb.placements.clone();
        chunks.sort_by_key(|chunk| chunk.chunk_index);

        let total_uncompressed: u32 = chunks.iter().map(|chunk| chunk.uncompressed_size).sum();
        let header = XorbChunkSequenceHeader::new(xorb.hash, chunks.len(), total_uncompressed);
        let entries: Vec<XorbChunkSequenceEntry> = chunks
            .iter()
            .scan(0u32, |byte_offset, chunk| {
                let entry = XorbChunkSequenceEntry::new(
                    chunk.chunk_hash,
                    chunk.uncompressed_size,
                    *byte_offset,
                );
                *byte_offset = byte_offset.saturating_add(chunk.uncompressed_size);
                Some(entry)
            })
            .collect();

        let xorb_info = Arc::new(MDBXorbInfo {
            metadata: header,
            chunks: entries,
        });
        if xorb_info_by_hash.insert(xorb.hash, xorb_info).is_some() {
            return Err(AuthServerError::Internal(format!(
                "duplicate protected-view xorb metadata for {}",
                xorb.hash.hex()
            )));
        }
    }

    let mut ordered_files = files.iter().collect::<Vec<_>>();
    ordered_files.sort_by_key(|file| file.file_hash);
    for file in ordered_files {
        let terms = build_file_terms(&file.file_hash, &file.chunk_hashes, placement)?;
        let covered: u64 = terms
            .iter()
            .map(|term| u64::from(term.unpacked_bytes))
            .sum();
        if covered != file.size {
            return Err(AuthServerError::IncompleteShardReconstruction {
                file_hash: file.file_hash.hex(),
                path: None,
                uncovered_chunks: 0,
                example_chunk_hash: String::new(),
                example_chunk_index: u32::MAX,
            });
        }

        let entries: Vec<FileDataSequenceEntry> = terms
            .iter()
            .map(|term| {
                FileDataSequenceEntry::new(
                    term.xorb_hash,
                    term.unpacked_bytes,
                    term.chunk_start,
                    term.chunk_end,
                )
            })
            .collect();
        let mut dependency_hashes = entries
            .iter()
            .map(|entry| entry.xorb_hash)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        dependency_hashes.sort_unstable();
        let dependencies = dependency_hashes
            .into_iter()
            .map(|xorb_hash| {
                xorb_info_by_hash.get(&xorb_hash).cloned().ok_or_else(|| {
                    AuthServerError::Internal(format!(
                        "missing protected-view xorb metadata {} for file {}",
                        xorb_hash.hex(),
                        file.file_hash.hex()
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let header = FileDataSequenceHeader::new(file.file_hash, entries.len(), false, false);
        shard_session.add_file_bundle(
            MDBFileInfo {
                metadata: header,
                segments: entries,
                verification: vec![],
                metadata_ext: None,
            },
            &dependencies,
        )?;
    }

    Ok(ViewShardPlan {
        shards: shard_session.finalize()?,
    })
}

pub(super) async fn commit_view_metadb(
    store: &Store,
    router: &StoreLayout<Store>,
    uploaded: &UploadedViewCrabObjects,
    manifest: &crab_metadata::manifests::Manifest,
    gc_registry_generation: u64,
) -> Result<[u8; 32]> {
    let shard_index_hash = MerkleHash::from_hex(&manifest.shard_index_hash)
        .map_err(|error| AuthServerError::Internal(format!("invalid view shard index: {error}")))?;
    let config = RemoteIndexConfig::for_repo_with_global_prefix(
        router.repo_prefix(),
        router.global_prefix(),
    );
    let digest = generation_file_index_digest(shard_index_hash.into());
    let writer = RemoteIndexWriter::open(Arc::clone(store.inner()), &config, true, true).await?;
    let workspace = tempfile::tempdir()?;
    let operation = async {
        let mut origins: HashMap<MerkleHash, OriginReceipt> = HashMap::new();
        let mut seen_chunks = HashSet::new();
        for (bytes, shard_hash) in &uploaded.shards {
            let spool = ShardReplaySpool::from_reader_in(
                std::io::Cursor::new(bytes),
                workspace.path(),
                *shard_hash,
                true,
                true,
            )?;
            let mut after_id = 0_i64;
            loop {
                let rows = spool.file_batch(after_id, REPLAY_BATCH_ENTRIES)?;
                if rows.is_empty() {
                    break;
                }
                let entries = rows
                    .into_iter()
                    .map(|row| {
                        after_id = row.id;
                        (
                            row.file_hash,
                            CommittedFileRecord {
                                recipe_hash: row.recipe_hash,
                                shard_hash: *shard_hash,
                                committed_generation: manifest.generation,
                                shard_index_hash,
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                writer.write_entries(&entries, &[]).await?;
            }

            let mut after_id = 0_i64;
            loop {
                let rows = spool.chunk_batch(after_id, REPLAY_BATCH_ENTRIES)?;
                if rows.is_empty() {
                    break;
                }
                let mut entries = Vec::with_capacity(rows.len());
                for row in rows {
                    after_id = row.id;
                    if !seen_chunks.insert(row.chunk_hash) {
                        continue;
                    }
                    let placement = uploaded.placement.get(&row.chunk_hash).ok_or_else(|| {
                        AuthServerError::IncompleteShardReconstruction {
                            file_hash: String::new(),
                            path: None,
                            uncovered_chunks: 1,
                            example_chunk_hash: row.chunk_hash.hex(),
                            example_chunk_index: row.chunk_index,
                        }
                    })?;
                    if placement.xorb_hash != row.xorb_hash
                        || placement.chunk_index != row.chunk_index
                        || placement.uncompressed_size != row.uncompressed_size
                    {
                        return Err(AuthServerError::CorruptObject {
                            path: format!("view shard {}", shard_hash.hex()),
                            reason: format!(
                                "chunk {} placement differs from uploaded xorb",
                                row.chunk_hash.hex()
                            ),
                        });
                    }
                    let origin = if let Some(origin) = origins.get(&row.xorb_hash) {
                        origin.clone()
                    } else {
                        let path = router.xorb_path(&row.xorb_hash);
                        let meta = store.head(&path).await?;
                        let payload_digest = uploaded
                            .payload_digests
                            .get(&row.xorb_hash)
                            .copied()
                            .ok_or_else(|| {
                                AuthServerError::Internal(format!(
                                    "missing payload digest for view xorb {}",
                                    row.xorb_hash.hex()
                                ))
                            })?;
                        let origin = OriginReceipt::new(
                            "canonical-origin".to_owned(),
                            path.to_string(),
                            row.xorb_hash.into(),
                            payload_digest,
                            meta.size,
                            meta.e_tag,
                            meta.version,
                        );
                        origins.insert(row.xorb_hash, origin.clone());
                        origin
                    };
                    entries.push((
                        row.chunk_hash,
                        CommittedChunkReceipt {
                            schema_version: RECEIPT_SCHEMA_VERSION,
                            chunk_hash: row.chunk_hash.into(),
                            xorb_hash: row.xorb_hash.into(),
                            chunk_index: row.chunk_index,
                            uncompressed_size: row.uncompressed_size,
                            origin,
                            source_repo_prefix: router.repo_prefix().to_owned(),
                            source_shard_hash: (*shard_hash).into(),
                            committed_generation: manifest.generation,
                            shard_index_hash: shard_index_hash.into(),
                            gc_registry_generation,
                        },
                    ));
                }
                writer.write_entries(&[], &entries).await?;
            }
        }
        if seen_chunks.len() != uploaded.placement.len() {
            let missing = uploaded
                .placement
                .keys()
                .find(|hash| !seen_chunks.contains(hash))
                .copied()
                .ok_or_else(|| {
                    AuthServerError::Internal("view chunk coverage count mismatch".to_owned())
                })?;
            return Err(AuthServerError::IncompleteShardReconstruction {
                file_hash: String::new(),
                path: None,
                uncovered_chunks: (uploaded.placement.len() - seen_chunks.len()) as u64,
                example_chunk_hash: missing.hex(),
                example_chunk_index: uploaded.placement[&missing].chunk_index,
            });
        }
        Ok::<_, AuthServerError>(())
    }
    .await;
    let close_result = writer.close().await;
    operation?;
    close_result?;
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crab_types::pointer::Pointer;
    use crab_xet::chunker::GearChunker;
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;

    use super::*;

    #[tokio::test]
    async fn upload_view_crab_objects_uses_view_local_global_prefix() {
        let store = Store::new(Arc::new(InMemory::new()));
        let repo_prefix = "org/repo/acl-views/v1/scope/view";
        let router = super::super::view_store_layout(&store, repo_prefix);
        crab_metadata::layout_descriptor::ensure_canonical_layout(&store, &router)
            .await
            .unwrap();
        let content = b"allowed bytes that stay behind a view-local pointer".to_vec();
        let file_hash = MerkleHash::from(*blake3::hash(&content).as_bytes());

        let mut chunker = GearChunker::new();
        let mut chunks = chunker.feed(&content);
        if let Some(last) = chunker.finalize() {
            chunks.push(last);
        }
        let chunk_hashes: Vec<MerkleHash> = chunks.iter().map(|chunk| chunk.hash).collect();
        let mut builder = XorbBuilder::new();
        for chunk in &chunks {
            builder.push(chunk, RunId(0)).unwrap();
        }
        let xorbs = builder.finalize().unwrap();
        let xorb_hash = xorbs[0].hash;
        let objects = ViewCrabObjects {
            files: vec![RepackedFile {
                file_hash,
                size: content.len() as u64,
                chunk_hashes,
            }],
            xorbs,
        };
        let uploaded = upload_view_crab_objects(&store, &router, objects)
            .await
            .unwrap();

        assert_eq!(uploaded.shard_hashes.len(), 1);
        assert!(
            store
                .head(&ObjectPath::from(format!(
                    "{repo_prefix}/.crab/xorbs/{}/{}",
                    &xorb_hash.hex()[..2],
                    xorb_hash.hex()
                )))
                .await
                .is_ok()
        );
        assert!(matches!(
            store
                .head(&ObjectPath::from(format!(
                    ".crab/xorbs/{}/{}",
                    &xorb_hash.hex()[..2],
                    xorb_hash.hex()
                )))
                .await,
            Err(crab_storage::StorageError::NotFound { .. })
        ));
        let (shard_index_hash, _, shard_index) =
            crab_metadata::manifests::compact_shard_index(1, &uploaded.shard_hashes).unwrap();
        crab_metadata::manifest_store::upload_segmented_bulk(
            &store,
            &router,
            &crab_metadata::manifests::BulkData {
                shard_index,
                pack_index: crab_metadata::segmented::SegmentWrite::default(),
            },
        )
        .await
        .unwrap();
        let mut manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.shard_index_hash = shard_index_hash;
        manifest.seal_git_validation();
        crab_metadata::manifest_store::create_manifest(&store, &router, &manifest)
            .await
            .unwrap();
        commit_view_metadb(&store, &router, &uploaded, &manifest, 1)
            .await
            .unwrap();
        let pointer = Pointer {
            file_hash: file_hash.into(),
            size: content.len() as u64,
            shard_hint: None,
        };
        super::super::verify_crab_pointers_backed_by_view(&store, repo_prefix, &[pointer])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn view_shard_forced_partitions_commit_metadb_dependency_closed() {
        use crab_xet::xorb::format::Chunk;

        let store = Store::new(Arc::new(InMemory::new()));
        let repo_prefix = "org/repo/acl-views/v1/scope/partitioned";
        let router = super::super::view_store_layout(&store, repo_prefix);
        let first = Chunk::new(Bytes::from_static(b"partitioned view first"));
        let second = Chunk::new(Bytes::from_static(b"partitioned view second"));
        let mut builder = XorbBuilder::new();
        builder.push(&first, RunId(0)).unwrap();
        builder.push(&second, RunId(1)).unwrap();
        let xorbs = builder.finalize().unwrap();
        assert_eq!(xorbs.len(), 1, "fixture must share one xorb dependency");

        let files = vec![
            RepackedFile {
                file_hash: MerkleHash::from(*blake3::hash(&first.data).as_bytes()),
                size: first.data.len() as u64,
                chunk_hashes: vec![first.hash],
            },
            RepackedFile {
                file_hash: MerkleHash::from(*blake3::hash(&second.data).as_bytes()),
                size: second.data.len() as u64,
                chunk_hashes: vec![second.hash],
            },
        ];
        let placement = placement_map(&xorbs);
        let plan = build_view_shards_with_session(
            &files,
            &xorbs,
            &placement,
            PushShardSession::with_qualification_size_cap(1),
        )
        .unwrap();
        assert_eq!(plan.shards.len(), 2);
        for (bytes, _) in &plan.shards {
            let recipes = crab_xet::shard_parse::extract_file_recipes(&Bytes::from(bytes.clone()))
                .expect("each protected-view partition must be dependency closed");
            assert_eq!(recipes.len(), 1);
        }

        for xorb in &xorbs {
            store
                .put(&router.xorb_path(&xorb.hash), xorb.bytes.clone())
                .await
                .unwrap();
        }
        for (bytes, hash) in &plan.shards {
            store
                .put(&router.shard_path(hash), Bytes::from(bytes.clone()))
                .await
                .unwrap();
        }
        let uploaded = UploadedViewCrabObjects {
            shard_hashes: plan.shards.iter().map(|(_, hash)| hash.hex()).collect(),
            shards: plan.shards,
            placement,
            payload_digests: xorbs
                .iter()
                .map(|xorb| (xorb.hash, xorb.payload_digest))
                .collect(),
        };
        let (shard_index_hash, _, shard_index) =
            crab_metadata::manifests::compact_shard_index(1, &uploaded.shard_hashes).unwrap();
        crab_metadata::manifest_store::upload_segmented_bulk(
            &store,
            &router,
            &crab_metadata::manifests::BulkData {
                shard_index,
                pack_index: crab_metadata::segmented::SegmentWrite::default(),
            },
        )
        .await
        .unwrap();
        let mut manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.shard_index_hash = shard_index_hash;
        manifest.seal_git_validation();
        crab_metadata::manifest_store::create_manifest(&store, &router, &manifest)
            .await
            .unwrap();

        commit_view_metadb(&store, &router, &uploaded, &manifest, 1)
            .await
            .expect("commit forced protected-view partitions");
    }
}
