//! Storage-backed adapter for segmented append-only pack/shard metadata.

use bytes::Bytes;
use crab_storage::{StorageError, Store, StoreLayout};
use serde::Deserialize;

use crate::error::{MetadataError, Result};
use crate::segmented::{
    SegmentIndex, SegmentKind, SegmentWrite, index_relative_path, parse_segment_index,
    parse_segment_records, validate_segment_index_shape,
};

fn content_hash(path: &object_store::path::Path, value: &str) -> Result<[u8; 32]> {
    blake3::Hash::from_hex(value)
        .map(|hash| *hash.as_bytes())
        .map_err(|error| MetadataError::CorruptObject {
            path: path.as_ref().to_owned(),
            reason: format!("invalid content hash: {error}"),
        })
}

/// Read a segment index by content hash.
pub async fn read_index(
    store: &Store,
    router: &StoreLayout<Store>,
    kind: SegmentKind,
    index_hash: &str,
) -> Result<SegmentIndex> {
    if index_hash.is_empty() {
        return Ok(SegmentIndex::default());
    }

    let path = router.repo_path(&index_relative_path(kind, index_hash));
    let expected = content_hash(&path, index_hash)?;
    let body = store.verify(&path, &expected).await?;
    let index = parse_segment_index(&body, path.as_ref())?;
    validate_segment_index_shape(kind, &index)?;
    Ok(index)
}

/// Read every record referenced by an ordered segment index.
pub async fn read_records<T: for<'de> Deserialize<'de>>(
    store: &Store,
    router: &StoreLayout<Store>,
    kind: SegmentKind,
    index_hash: &str,
) -> Result<Vec<T>> {
    let index = read_index(store, router, kind, index_hash).await?;
    let mut out = Vec::with_capacity(index.total_records as usize);
    for segment in &index.segments {
        let path = router.repo_path(&segment.path);
        let expected = content_hash(&path, &segment.hash)?;
        let body = store.verify(&path, &expected).await?;
        let mut records = parse_segment_records::<T>(kind, segment, &body, path.as_ref())?;
        out.append(&mut records);
    }
    Ok(out)
}

/// Upload an immutable segment or index if it is absent.
pub async fn upload_if_absent(
    store: &Store,
    router: &StoreLayout<Store>,
    relative_path: &str,
    bytes: &[u8],
) -> Result<()> {
    let path = router.repo_path(relative_path);
    match store.head(&path).await {
        Ok(_) => Ok(()),
        Err(StorageError::NotFound { .. }) => store.put(&path, Bytes::from(bytes.to_vec())).await,
        Err(e) => Err(e),
    }
    .map_err(MetadataError::from)
}

/// Upload all segment objects before their index object.
pub async fn upload_write(
    store: &Store,
    router: &StoreLayout<Store>,
    write: &SegmentWrite,
) -> Result<()> {
    for segment in &write.segments {
        upload_if_absent(store, router, &segment.reference.path, &segment.bytes).await?;
    }
    if let Some(index) = &write.index {
        upload_if_absent(
            store,
            router,
            &index_relative_path(index.kind, &index.hash),
            &index.bytes,
        )
        .await?;
    }
    Ok(())
}
