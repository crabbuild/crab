use bytes::Bytes;
use crab_storage::{ETag, StorageError, Store};
use object_store::path::Path;

use crate::{error::Result, validation::corrupt_object};

// Derived indexes can be rebuilt after corruption. Verify the replacement's
// content identity, then condition any repair on the exact corrupt version;
// neither object existence nor an unconditional overwrite proves correctness.
pub(crate) async fn upload(store: &Store, path: &Path, hash: &str, bytes: &[u8]) -> Result<()> {
    if blake3::hash(bytes).to_hex().as_str() != hash {
        return Err(corrupt_object(
            path.as_ref(),
            "derived index write does not match its content identity",
        ));
    }
    let bytes = Bytes::copy_from_slice(bytes);
    for attempt in 0..3 {
        match store.create_strict(path, bytes.clone()).await {
            Ok(()) => {}
            Err(StorageError::StateConflict { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        let etag = match store.get_with_etag_bounded(path, bytes.len() as u64).await {
            Ok((existing, _)) if existing == bytes => return Ok(()),
            Ok((_, etag)) => etag,
            Err(StorageError::NotFound { .. }) if attempt < 2 => continue,
            Err(error @ StorageError::CorruptObject { .. }) => {
                let meta = store.head(path).await?;
                if meta.size <= bytes.len() as u64 {
                    return Err(error.into());
                }
                ETag {
                    e_tag: meta.e_tag,
                    version: meta.version,
                }
            }
            Err(error) => return Err(error.into()),
        };
        match store.update(path, bytes.clone(), etag).await {
            Ok(_) => {
                let (stored, _) = store
                    .get_with_etag_bounded(path, bytes.len() as u64)
                    .await?;
                if stored != bytes {
                    return Err(corrupt_object(
                        path.as_ref(),
                        "derived index repair readback mismatch",
                    ));
                }
                return Ok(());
            }
            Err(StorageError::StateConflict { .. }) if attempt < 2 => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(corrupt_object(
        path.as_ref(),
        "derived index repair exhausted its conflict budget",
    ))
}
