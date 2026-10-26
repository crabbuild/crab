use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    FileCatalogEntry, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
};
use crab_storage::{Store, StoreLayout, content_hash_from_path};
use crab_xet::hash::MerkleHash;
use crab_xet::reconstruction::{ChunkPlacementMap, build_file_terms};
use crab_xet::shard::{
    FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, PushShardSession,
    ShardReader, XorbChunkSequenceEntry, XorbChunkSequenceHeader,
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
    catalog: PointerCatalog,
}

impl UploadedViewCrabObjects {
    pub(super) fn catalog(&self) -> &PointerCatalog {
        &self.catalog
    }
}

pub(super) async fn upload_view_crab_objects(
    store: &Store,
    router: &StoreLayout<Store>,
    objects: ViewCrabObjects,
) -> Result<UploadedViewCrabObjects> {
    let placement = placement_map(&objects.xorbs);
    let plan = build_view_shards(&objects.files, &objects.xorbs, &placement)?;
    let catalog = build_pointer_catalog(&objects, &plan)?;

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
        catalog,
    })
}

fn build_pointer_catalog(
    objects: &ViewCrabObjects,
    plan: &ViewShardPlan,
) -> Result<PointerCatalog> {
    let mut catalog = PointerCatalog::new();
    for xorb in &objects.xorbs {
        let mut placements = xorb.placements.iter().collect::<Vec<_>>();
        placements.sort_by_key(|placement| placement.chunk_index);
        catalog.insert_xorb(
            xorb.hash.hex(),
            XorbCatalogEntry::new(
                xorb.bytes.len() as u64,
                blake3::hash(&xorb.bytes).to_hex().to_string(),
                placements
                    .into_iter()
                    .map(|placement| {
                        XorbChunkEntry::new(placement.chunk_hash.hex(), placement.uncompressed_size)
                    })
                    .collect(),
            ),
        )?;
    }
    for (bytes, shard_hash) in &plan.shards {
        let mut xorb_hashes = crate::receive::strict_xorb_references_from_shard(bytes)?
            .keys()
            .map(|key| {
                content_hash_from_path(key, "xorbs")
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| {
                        AuthServerError::Internal(
                            "filtered view shard contains an invalid xorb key".to_owned(),
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        xorb_hashes.sort_unstable();
        catalog.insert_shard(
            shard_hash.hex(),
            ShardCatalogEntry::new(bytes.len() as u64, xorb_hashes),
        )?;
    }
    for file in &objects.files {
        let mut file_shard = None;
        for (bytes, shard_hash) in &plan.shards {
            let reader = ShardReader::from_bytes(Bytes::from(bytes.clone()), *shard_hash);
            if reader.get_file_info(&file.file_hash)?.is_some() {
                file_shard = Some(*shard_hash);
                break;
            }
        }
        let shard_hash = file_shard.ok_or_else(|| {
            AuthServerError::Internal(format!(
                "filtered view shard set omitted file {}",
                file.file_hash.hex()
            ))
        })?;
        catalog.insert_file(
            file.file_hash.hex(),
            FileCatalogEntry::new(file.size, shard_hash.hex()),
        )?;
    }
    catalog.encode()?;
    Ok(catalog)
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
        let repo_prefix = "org/repo/acl-views/v2/scope/view";
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
        assert_eq!(uploaded.catalog().files().len(), 1);
        assert_eq!(uploaded.catalog().shards().len(), 1);
        assert_eq!(uploaded.catalog().xorbs().len(), 1);
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
        let pointer = Pointer {
            file_hash: file_hash.into(),
            size: content.len() as u64,
            shard_hint: None,
        };
        super::super::verify_crab_pointers_backed_by_uploaded_view(
            &store,
            &router,
            &uploaded.shard_hashes,
            &[pointer],
        )
        .await
        .unwrap();
    }

    #[test]
    fn view_shard_partitions_preserve_file_and_xorb_coverage() {
        use crab_xet::xorb::format::Chunk;

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

        let objects = ViewCrabObjects { files, xorbs };
        let catalog = build_pointer_catalog(&objects, &plan).unwrap();
        assert_eq!(catalog.files().len(), 2);
        assert_eq!(catalog.shards().len(), 2);
        assert_eq!(catalog.xorbs().len(), 1);
        catalog
            .encode()
            .expect("partitioned catalog must retain complete dependencies");
    }
}
