use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use crab_metadata::receipts::{
    CommittedChunkReceipt, OriginReceipt, RECEIPT_SCHEMA_VERSION, generation_file_index_digest,
};
use crab_metadata::remote_index::{RemoteIndexConfig, write_index_entries};
use crab_metadata::value_codec::CommittedFileRecord;
use crab_staging::recipe::{ChunkingPolicyId, FileRecipe};
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
    let mut shard_session = PushShardSession::new();
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

        shard_session.add_xorb(Arc::new(MDBXorbInfo {
            metadata: header,
            chunks: entries,
        }))?;
    }

    for file in files {
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
        let header = FileDataSequenceHeader::new(file.file_hash, entries.len(), false, false);
        shard_session.add_file(MDBFileInfo {
            metadata: header,
            segments: entries,
            verification: vec![],
            metadata_ext: None,
        })?;
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
    let mut file_entries = Vec::new();
    let mut chunk_source_shards = HashMap::new();
    for (bytes, shard_hash) in &uploaded.shards {
        for recipe in crab_xet::shard_parse::extract_file_recipes(&Bytes::from(bytes.clone()))? {
            let file_size = recipe.chunks.iter().try_fold(0u64, |total, (_, size)| {
                total.checked_add(*size).ok_or_else(|| {
                    AuthServerError::Internal("view recipe size overflow".to_owned())
                })
            })?;
            let recipe_hash = FileRecipe::from_staged_chunks(
                ChunkingPolicyId::XetGearV1_64KiB,
                recipe.file_hash,
                file_size,
                &recipe.chunks,
            )
            .map_err(|error| AuthServerError::Internal(error.to_string()))?
            .hash();
            file_entries.push((
                recipe.file_hash,
                CommittedFileRecord {
                    recipe_hash,
                    shard_hash: *shard_hash,
                    committed_generation: manifest.generation,
                    shard_index_hash,
                },
            ));
            for (chunk_hash, _) in recipe.chunks {
                chunk_source_shards.entry(chunk_hash).or_insert(*shard_hash);
            }
        }
    }
    let mut origins: HashMap<MerkleHash, OriginReceipt> = HashMap::new();
    let mut committed_chunk_entries = Vec::with_capacity(uploaded.placement.len());
    for (chunk_hash, entry) in &uploaded.placement {
        let source_shard_hash = chunk_source_shards.get(chunk_hash).ok_or_else(|| {
            AuthServerError::IncompleteShardReconstruction {
                file_hash: String::new(),
                path: None,
                uncovered_chunks: 1,
                example_chunk_hash: chunk_hash.hex(),
                example_chunk_index: entry.chunk_index,
            }
        })?;
        let origin = if let Some(origin) = origins.get(&entry.xorb_hash) {
            origin.clone()
        } else {
            let path = router.xorb_path(&entry.xorb_hash);
            let meta = store.head(&path).await?;
            let payload_digest = uploaded
                .payload_digests
                .get(&entry.xorb_hash)
                .copied()
                .ok_or_else(|| {
                    AuthServerError::Internal(format!(
                        "missing payload digest for view xorb {}",
                        entry.xorb_hash.hex()
                    ))
                })?;
            let origin = OriginReceipt::new(
                "canonical-origin".to_owned(),
                path.to_string(),
                entry.xorb_hash.into(),
                payload_digest,
                meta.size,
                meta.e_tag,
                meta.version,
            );
            origins.insert(entry.xorb_hash, origin.clone());
            origin
        };
        committed_chunk_entries.push((
            *chunk_hash,
            CommittedChunkReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                chunk_hash: (*chunk_hash).into(),
                xorb_hash: entry.xorb_hash.into(),
                chunk_index: entry.chunk_index,
                uncompressed_size: entry.uncompressed_size,
                origin,
                source_repo_prefix: router.repo_prefix().to_owned(),
                source_shard_hash: (*source_shard_hash).into(),
                committed_generation: manifest.generation,
                shard_index_hash: shard_index_hash.into(),
                gc_registry_generation,
            },
        ));
    }
    let config = RemoteIndexConfig::for_repo_with_global_prefix(
        router.repo_prefix(),
        router.global_prefix(),
    );

    let digest = generation_file_index_digest(shard_index_hash.into());
    write_index_entries(
        Arc::clone(store.inner()),
        &config,
        &file_entries,
        &committed_chunk_entries,
    )
    .await?;
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
}
