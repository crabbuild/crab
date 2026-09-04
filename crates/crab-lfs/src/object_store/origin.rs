use std::sync::{Arc, LazyLock};

use tokio::sync::Semaphore;

use super::*;

static VERIFICATIONS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(4)));

pub(super) async fn inspect(
    store: &Store,
    path: &Path,
    oid: &[u8; 32],
    expected_size: Option<u64>,
) -> Result<ExistingObject> {
    // Acquire before opening the body; each detached hash job shares ownership
    // so cancellation cannot admit replacement buffers before that job exits.
    let permit = Arc::new(
        Arc::clone(&VERIFICATIONS)
            .acquire_owned()
            .await
            .map_err(std::io::Error::other)?,
    );
    let (meta, range, mut stream) = match store.get_stream(path, None).await {
        Ok(result) => result,
        Err(StorageError::NotFound { .. }) => return Ok(ExistingObject::Missing),
        Err(error) => return Err(error.into()),
    };
    let etag = ETag {
        e_tag: meta.e_tag.clone(),
        version: meta.version.clone(),
    };
    // Invalid transport framing is not evidence that the stored object should
    // be repaired. Keep it separate from a complete body's identity mismatch.
    if range.start != 0 || range.end != meta.size {
        return Err(StorageError::CorruptObject {
            path: path.to_string(),
            reason: "incomplete LFS object response range".to_owned(),
        }
        .into());
    }
    if expected_size.is_some_and(|size| size != meta.size) {
        return Ok(ExistingObject::Corrupt(etag));
    }
    let mut hasher = Sha256::new();
    let mut actual_size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let size = actual_size.checked_add(chunk.len() as u64).ok_or_else(|| {
            StorageError::CorruptObject {
                path: path.to_string(),
                reason: "object size overflow while verifying LFS content".to_owned(),
            }
        })?;
        if size > meta.size {
            return Err(StorageError::CorruptObject {
                path: path.to_string(),
                reason: "LFS object body exceeds response size".to_owned(),
            }
            .into());
        }
        actual_size = size;
        let permit = Arc::clone(&permit);
        hasher = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            hasher.update(&chunk);
            hasher
        })
        .await
        .map_err(std::io::Error::other)?;
    }
    if actual_size != meta.size {
        return Err(StorageError::CorruptObject {
            path: path.to_string(),
            reason: format!(
                "incomplete LFS object body: expected {} bytes, read {actual_size}",
                meta.size
            ),
        }
        .into());
    }
    if hasher.finalize().as_slice() == oid {
        Ok(ExistingObject::Valid(meta))
    } else {
        Ok(ExistingObject::Corrupt(etag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;
    use object_store::{ObjectStore, memory::InMemory};
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    #[tokio::test]
    async fn origin_rehashes_even_a_matching_receipt_without_mutation() {
        let storage = Arc::new(InMemory::new());
        let store = Store::new(storage.clone());
        let lfs = LfsObjectStore::new(store.clone(), "origin");
        let oid: [u8; 32] = Sha256::digest(b"hello").into();
        let path = lfs.object_path(&oid);
        store
            .put(&path, Bytes::from_static(b"wrong"))
            .await
            .unwrap();
        let meta = store.head(&path).await.unwrap();
        // A receipt with the current validator cannot substitute for hashing
        // during origin preflight, even if its content claims verification.
        LfsObjectStore::record_verification_receipt_with_meta(&store, "origin", &oid, &meta).await;
        assert!(receipt_matches(&store, "origin", &path, &oid, &meta).await);
        let before = storage
            .list(None)
            .map_ok(|meta| (meta.location, (meta.e_tag, meta.size)))
            .try_collect::<std::collections::BTreeMap<_, _>>()
            .await
            .unwrap();
        let result = lfs.verify_origin(&oid, 5).await;
        let after = storage
            .list(None)
            .map_ok(|meta| (meta.location, (meta.e_tag, meta.size)))
            .try_collect::<std::collections::BTreeMap<_, _>>()
            .await
            .unwrap();
        assert_eq!(
            (matches!(result, Err(LfsError::ObjectCorrupt { .. })), after),
            (true, before)
        );
    }

    #[tokio::test]
    async fn origin_checks_size_before_body_and_never_uses_primary_fallback() {
        let storage = Arc::new(InMemory::new());
        let bytes = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&bytes);
        let store = Store::new(storage).with_read_byte_observer(Arc::new(move |count| {
            observed.fetch_add(count, Ordering::Relaxed);
        }));
        let oid: [u8; 32] = Sha256::digest(b"hello").into();
        let lfs = LfsObjectStore::new(store.clone(), "origin");
        store
            .put(&lfs.object_path(&oid), Bytes::from_static(b"hello"))
            .await
            .unwrap();
        let wrong_size = lfs.verify_origin(&oid, 1).await;
        let fallback = LfsObjectStore::new_with_primary_fallback(
            Store::new(Arc::new(InMemory::new())),
            "empty",
            store,
            "origin",
        );
        let missing = fallback.verify_origin(&oid, 5).await;
        assert_eq!(
            (
                matches!(wrong_size, Err(LfsError::ObjectCorrupt { .. })),
                matches!(missing, Err(LfsError::ObjectMissing { .. })),
                bytes.load(Ordering::Relaxed)
            ),
            (true, true, 0)
        );
    }

    #[tokio::test]
    async fn origin_deadline_can_cancel_admission_without_reads() {
        let store = Store::new(Arc::new(InMemory::new()));
        let lfs = LfsObjectStore::new(store, "origin");
        let all = Arc::clone(&VERIFICATIONS)
            .acquire_many_owned(4)
            .await
            .unwrap();
        let result =
            tokio::time::timeout(Duration::from_millis(10), lfs.verify_origin(&[0; 32], 0)).await;
        drop(all);
        assert!(result.is_err());
    }
}
