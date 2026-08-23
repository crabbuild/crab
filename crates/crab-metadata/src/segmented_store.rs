//! Storage-backed adapter for segmented append-only pack/shard metadata.

use bytes::Bytes;
use crab_storage::{StorageError, Store, StoreLayout};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::pin::Pin;

use crate::error::{MetadataError, Result};
use crate::segmented::{
    SegmentIndex, SegmentKind, SegmentWrite, index_relative_path, parse_segment_index,
    parse_segment_records, validate_segment_index_shape,
};

const MAX_SEGMENT_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MAX_STREAMED_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

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
    if body.len() > MAX_SEGMENT_INDEX_BYTES {
        return Err(MetadataError::CorruptObject {
            path: path.as_ref().to_owned(),
            reason: format!(
                "metadata segment index is {} bytes, above the {}-byte budget",
                body.len(),
                MAX_SEGMENT_INDEX_BYTES
            ),
        });
    }
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

/// Visit records in an ordered segment index without retaining the complete
/// collection. Each segment is decoded and released before the next segment
/// is fetched; callers can persist or otherwise consume one record at a time.
pub async fn visit_records<T, F, Fut, E>(
    store: &Store,
    router: &StoreLayout<Store>,
    kind: SegmentKind,
    index_hash: &str,
    mut visit: F,
) -> std::result::Result<(), E>
where
    T: DeserializeOwned,
    F: FnMut(T) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<(), E>>,
    E: From<MetadataError>,
{
    let index = read_index(store, router, kind, index_hash)
        .await
        .map_err(E::from)?;
    for segment in index.segments {
        if segment.bytes > MAX_STREAMED_SEGMENT_BYTES {
            return Err(E::from(MetadataError::CorruptObject {
                path: segment.path.clone(),
                reason: format!(
                    "metadata segment is {} bytes, above the {}-byte streaming budget",
                    segment.bytes, MAX_STREAMED_SEGMENT_BYTES
                ),
            }));
        }
        let path = router.repo_path(&segment.path);
        let expected = content_hash(&path, &segment.hash).map_err(E::from)?;
        let body = store
            .verify(&path, &expected)
            .await
            .map_err(|error| E::from(MetadataError::from(error)))?;
        if body.len() as u64 > MAX_STREAMED_SEGMENT_BYTES {
            return Err(E::from(MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "metadata segment body is {} bytes, above the {}-byte streaming budget",
                    body.len(),
                    MAX_STREAMED_SEGMENT_BYTES
                ),
            }));
        }
        let records =
            parse_segment_records::<T>(kind, &segment, &body, path.as_ref()).map_err(E::from)?;
        for record in records {
            visit(record).await?;
        }
    }
    Ok(())
}

/// Async record visitor whose future borrows the visitor for one record.
pub trait AsyncRecordVisitor<T, E> {
    /// Consume one validated record.
    fn visit<'a>(
        &'a mut self,
        record: T,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), E>> + 'a>>;
}

/// Async visitor variant that does not force callers to collect a complete
/// segment when each record must be persisted to another async sink.
pub async fn visit_records_async<T, V, E>(
    store: &Store,
    router: &StoreLayout<Store>,
    kind: SegmentKind,
    index_hash: &str,
    visitor: &mut V,
) -> std::result::Result<(), E>
where
    T: DeserializeOwned,
    V: AsyncRecordVisitor<T, E>,
    E: From<MetadataError>,
{
    let index = read_index(store, router, kind, index_hash)
        .await
        .map_err(E::from)?;
    for segment in index.segments {
        if segment.bytes > MAX_STREAMED_SEGMENT_BYTES {
            return Err(E::from(MetadataError::CorruptObject {
                path: segment.path.clone(),
                reason: format!(
                    "metadata segment is {} bytes, above the {}-byte streaming budget",
                    segment.bytes, MAX_STREAMED_SEGMENT_BYTES
                ),
            }));
        }
        let path = router.repo_path(&segment.path);
        let expected = content_hash(&path, &segment.hash).map_err(E::from)?;
        let body = store
            .verify(&path, &expected)
            .await
            .map_err(|error| E::from(MetadataError::from(error)))?;
        if body.len() as u64 > MAX_STREAMED_SEGMENT_BYTES {
            return Err(E::from(MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "metadata segment body is {} bytes, above the {}-byte streaming budget",
                    body.len(),
                    MAX_STREAMED_SEGMENT_BYTES
                ),
            }));
        }
        let records =
            parse_segment_records::<T>(kind, &segment, &body, path.as_ref()).map_err(E::from)?;
        for record in records {
            visitor.visit(record).await?;
        }
    }
    Ok(())
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
