use bytes::Bytes;
use crab_storage::{StorageError, Store, StoreLayout};

use super::{
    PathStateCheckpoint, PathStateDescriptor, PathStateIndex, PathStateWrite, Result, corrupt_at,
    corruption, decode_layer, path_state_checkpoint_path, validate_descriptor_shape,
};
use crate::{
    error::MetadataError, split_commit_graph::SplitCommitGraph, validation::validate_content_hash,
};

/// Load and verify only the path-state descriptor.
pub async fn load_path_state_descriptor(
    store: &Store,
    layout: &StoreLayout<Store>,
    descriptor_hash: &str,
    max_bytes: u64,
) -> Result<PathStateDescriptor> {
    load_path_state_descriptor_with_size(store, layout, descriptor_hash, max_bytes)
        .await
        .map(|(descriptor, _)| descriptor)
}

async fn load_path_state_descriptor_with_size(
    store: &Store,
    layout: &StoreLayout<Store>,
    descriptor_hash: &str,
    max_bytes: u64,
) -> Result<(PathStateDescriptor, u64)> {
    validate_content_hash(
        descriptor_hash,
        "path-state descriptor hash",
        "path-state descriptor",
    )?;
    let descriptor_path = layout.bulk_manifest_path("path-state", descriptor_hash);
    let expected = decode_hash(descriptor_hash, descriptor_path.as_ref())?;
    let descriptor_bytes = store.verify(&descriptor_path, &expected).await?;
    if descriptor_bytes.len() as u64 > max_bytes {
        return corrupt_at(
            descriptor_path.as_ref(),
            "path-state descriptor exceeds its byte limit",
        );
    }
    let descriptor_bytes_len = descriptor_bytes.len() as u64;
    let descriptor = serde_json::from_slice(&descriptor_bytes).map_err(|source| {
        MetadataError::CorruptObject {
            path: descriptor_path.to_string(),
            reason: format!("invalid path-state descriptor: {source}"),
        }
    })?;
    validate_descriptor_shape(&descriptor)?;
    Ok((descriptor, descriptor_bytes_len))
}

/// Load and verify a complete path-state index.
pub async fn load_path_state(
    store: &Store,
    layout: &StoreLayout<Store>,
    descriptor_hash: &str,
    graph: &SplitCommitGraph,
    max_bytes: u64,
) -> Result<PathStateIndex> {
    load_path_state_bound(store, layout, descriptor_hash, graph, max_bytes, true).await
}

async fn load_path_state_prefix(
    store: &Store,
    layout: &StoreLayout<Store>,
    descriptor_hash: &str,
    graph: &SplitCommitGraph,
    max_bytes: u64,
) -> Result<PathStateIndex> {
    load_path_state_bound(store, layout, descriptor_hash, graph, max_bytes, false).await
}

async fn load_path_state_bound(
    store: &Store,
    layout: &StoreLayout<Store>,
    descriptor_hash: &str,
    graph: &SplitCommitGraph,
    max_bytes: u64,
    complete: bool,
) -> Result<PathStateIndex> {
    let (descriptor, mut fetched) =
        load_path_state_descriptor_with_size(store, layout, descriptor_hash, max_bytes).await?;
    let descriptor_path = layout.bulk_manifest_path("path-state", descriptor_hash);
    let mut layers = Vec::new();
    layers
        .try_reserve_exact(descriptor.layers.len())
        .map_err(|_| corruption("path-state layer allocation exceeds capacity"))?;
    for reference in &descriptor.layers {
        fetched = fetched
            .checked_add(reference.bytes)
            .ok_or_else(|| corruption("path-state byte count overflows"))?;
        if fetched > max_bytes {
            return corrupt_at(
                descriptor_path.as_ref(),
                "path-state index exceeds its byte limit",
            );
        }
        let path = layout.repo_path(&reference.path);
        let expected = decode_hash(&reference.hash, path.as_ref())?;
        let bytes = store.verify(&path, &expected).await?;
        if bytes.len() as u64 != reference.bytes {
            return corrupt_at(path.as_ref(), "path-state layer length mismatch");
        }
        layers.push(decode_layer(&bytes, reference, path.as_ref())?);
    }
    if complete {
        PathStateIndex::new(descriptor, layers, graph)
    } else {
        PathStateIndex::new_prefix(descriptor, layers, graph)
    }
}

/// Load a verified resumable prefix for the current commit graph.
pub async fn load_path_state_checkpoint(
    store: &Store,
    layout: &StoreLayout<Store>,
    graph: &SplitCommitGraph,
    max_bytes: u64,
) -> Result<Option<PathStateIndex>> {
    let Some(checkpoint) =
        load_path_state_checkpoint_record(store, layout, &graph.descriptor.git_validation_digest)
            .await?
    else {
        return Ok(None);
    };
    if !checkpoint_matches(&checkpoint, graph) {
        return Ok(None);
    }
    let index =
        load_path_state_prefix(store, layout, &checkpoint.descriptor_hash, graph, max_bytes)
            .await?;
    if index.descriptor.commit_count != checkpoint.commit_count {
        let path = layout.repo_path(&path_state_checkpoint_path(
            &graph.descriptor.git_validation_digest,
        ));
        return corrupt_at(
            path.as_ref(),
            "path-state checkpoint count does not match its descriptor",
        );
    }
    Ok(Some(index))
}

/// Load and validate the bounded mutable checkpoint record without its layers.
pub async fn load_path_state_checkpoint_record(
    store: &Store,
    layout: &StoreLayout<Store>,
    git_validation_digest: &str,
) -> Result<Option<PathStateCheckpoint>> {
    validate_content_hash(
        git_validation_digest,
        "path-state checkpoint Git validation digest",
        "path-state checkpoint",
    )?;
    let path = layout.repo_path(&path_state_checkpoint_path(git_validation_digest));
    let bytes = match store.get_with_etag_bounded(&path, 64 * 1024).await {
        Ok((bytes, _)) => bytes,
        Err(StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let checkpoint: PathStateCheckpoint =
        serde_json::from_slice(&bytes).map_err(|source| MetadataError::CorruptObject {
            path: path.to_string(),
            reason: format!("invalid path-state checkpoint: {source}"),
        })?;
    validate_content_hash(
        &checkpoint.descriptor_hash,
        "path-state checkpoint descriptor hash",
        path.as_ref(),
    )?;
    if checkpoint.version != 1 || checkpoint.git_validation_digest != git_validation_digest {
        return corrupt_at(path.as_ref(), "path-state checkpoint identity is invalid");
    }
    Ok(Some(checkpoint))
}

/// Publish resumable progress after its immutable descriptor is durable.
///
/// `replace_descriptor_hash` permits regression only from a checkpoint the
/// caller already failed to verify; ordinary writers remain monotonic.
pub async fn publish_path_state_checkpoint(
    store: &Store,
    layout: &StoreLayout<Store>,
    graph: &SplitCommitGraph,
    descriptor_hash: &str,
    commit_count: u32,
    replace_descriptor_hash: Option<&str>,
) -> Result<()> {
    validate_content_hash(
        descriptor_hash,
        "path-state checkpoint descriptor hash",
        "path-state checkpoint",
    )?;
    let checkpoint = PathStateCheckpoint {
        version: 1,
        generation: graph.descriptor.generation,
        pack_index_hash: graph.descriptor.pack_index_hash.clone(),
        git_validation_digest: graph.descriptor.git_validation_digest.clone(),
        commit_ordinal_digest: graph.ordinal_digest(),
        commit_count,
        descriptor_hash: descriptor_hash.to_owned(),
    };
    let bytes = Bytes::from(serde_json::to_vec(&checkpoint).map_err(|source| {
        MetadataError::Internal(format!("path-state checkpoint encode: {source}"))
    })?);
    let path = layout.repo_path(&path_state_checkpoint_path(
        &graph.descriptor.git_validation_digest,
    ));
    for attempt in 0..3 {
        match store.get_with_etag_bounded(&path, 64 * 1024).await {
            Ok((current, etag)) => {
                if serde_json::from_slice::<PathStateCheckpoint>(&current)
                    .ok()
                    .filter(|current| checkpoint_matches(current, graph))
                    .is_some_and(|current| {
                        current.commit_count >= commit_count
                            && replace_descriptor_hash != Some(current.descriptor_hash.as_str())
                    })
                {
                    return Ok(());
                }
                match store.update(&path, bytes.clone(), etag).await {
                    Ok(_) => return Ok(()),
                    Err(StorageError::StateConflict { .. }) if attempt < 2 => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(StorageError::NotFound { .. }) => {
                match store.create_strict(&path, bytes.clone()).await {
                    Ok(()) => return Ok(()),
                    Err(StorageError::StateConflict { .. }) if attempt < 2 => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(MetadataError::Internal(
        "path-state checkpoint CAS retries exhausted".to_owned(),
    ))
}

fn checkpoint_matches(checkpoint: &PathStateCheckpoint, graph: &SplitCommitGraph) -> bool {
    checkpoint.version == 1
        && checkpoint.generation == graph.descriptor.generation
        && checkpoint.pack_index_hash == graph.descriptor.pack_index_hash
        && checkpoint.git_validation_digest == graph.descriptor.git_validation_digest
        && checkpoint.commit_ordinal_digest == graph.ordinal_digest()
        && checkpoint.commit_count <= graph.descriptor.commit_count
}

/// Upload changed layers before the immutable descriptor.
pub async fn upload_path_state(
    store: &Store,
    layout: &StoreLayout<Store>,
    write: &PathStateWrite,
) -> Result<()> {
    for layer in &write.layers {
        upload_if_absent(
            store,
            &layout.repo_path(&layer.reference.path),
            &layer.bytes,
        )
        .await?;
    }
    upload_if_absent(
        store,
        &layout.bulk_manifest_path("path-state", &write.descriptor_hash),
        &write.descriptor_bytes,
    )
    .await
}

async fn upload_if_absent(
    store: &Store,
    path: &object_store::path::Path,
    bytes: &[u8],
) -> Result<()> {
    match store.head(path).await {
        Ok(_) => Ok(()),
        Err(StorageError::NotFound { .. }) => {
            store.put(path, Bytes::copy_from_slice(bytes)).await?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn decode_hash(value: &str, path: &str) -> Result<[u8; 32]> {
    blake3::Hash::from_hex(value)
        .map(|hash| *hash.as_bytes())
        .map_err(|source| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: format!("invalid content hash: {source}"),
        })
}
