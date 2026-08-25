//! Version-bound integrity receipts for committed Git pack bodies.

use bytes::Bytes;
use object_store::ObjectMeta;

use crate::error::{MetadataError, Result};
use crate::manifests::PackManifestEntry;
use crate::receipts::OriginReceipt;
use crab_storage::{StorageError, Store, StoreLayout};

const PACK_ORIGIN_NAMESPACE: &str = "canonical-origin";
const MAX_STABILITY_ATTEMPTS: usize = 3;
const MAX_ORIGIN_RECEIPT_BYTES: u64 = 8 * 1024 * 1024;

fn corrupt(path: &str, reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

fn has_version_token(meta: &ObjectMeta) -> bool {
    meta.e_tag.is_some() || meta.version.is_some()
}

fn receipt_matches_meta(receipt: &OriginReceipt, meta: &ObjectMeta) -> bool {
    has_version_token(meta) && receipt.etag == meta.e_tag && receipt.object_version == meta.version
}

fn same_object_version(left: &ObjectMeta, right: &ObjectMeta) -> bool {
    if has_version_token(left) || has_version_token(right) {
        left.e_tag == right.e_tag && left.version == right.version
    } else {
        left.size == right.size
    }
}

async fn read_matching_receipt(
    store: &Store,
    receipt_path: &object_store::path::Path,
    pack_path: &object_store::path::Path,
    expected_hash: [u8; 32],
    expected_size: u64,
    meta: &ObjectMeta,
) -> Result<bool> {
    let body = match store
        .get_with_etag_bounded(receipt_path, MAX_ORIGIN_RECEIPT_BYTES)
        .await
    {
        Ok((body, _)) => body,
        Err(StorageError::NotFound { .. }) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let receipt: OriginReceipt = match serde_json::from_slice(&body) {
        Ok(receipt) => receipt,
        Err(error) => {
            tracing::warn!(
                path = %receipt_path,
                error = %error,
                "ignoring corrupt pack-origin receipt and revalidating the pack body"
            );
            return Ok(false);
        }
    };
    if receipt
        .validate(
            PACK_ORIGIN_NAMESPACE,
            pack_path.as_ref(),
            expected_hash,
            expected_size,
        )
        .is_err()
    {
        tracing::warn!(
            path = %receipt_path,
            "ignoring pack-origin receipt for a different object identity"
        );
        return Ok(false);
    }
    Ok(receipt_matches_meta(&receipt, meta))
}

/// Persist a version-bound receipt after the caller has verified the bytes it
/// wrote. Scoped protected-push clients skip the canonical receipt; the
/// receive service records it after promotion.
pub async fn record_verified_pack_origin(
    store: &Store,
    repo_prefix: &str,
    pack: &PackManifestEntry,
) -> Result<()> {
    let router = StoreLayout::new(store.clone(), repo_prefix.to_owned());
    if store.staging_write_prefix().is_some() {
        return Ok(());
    }
    if pack.pack_id != pack.content_hash {
        return Err(corrupt(
            router.pack_path(&pack.pack_id).as_ref(),
            "pack manifest identity differs from its content hash",
        ));
    }
    let expected = blake3::Hash::from_hex(&pack.content_hash).map_err(|error| {
        corrupt(
            router.pack_path(&pack.pack_id).as_ref(),
            format!("invalid pack content hash: {error}"),
        )
    })?;
    let pack_path = router.pack_path(&pack.pack_id);
    let meta = store.head(&pack_path).await?;
    if meta.size != pack.size {
        return Err(corrupt(
            pack_path.as_ref(),
            format!(
                "verified pack upload has {} bytes, expected {}",
                meta.size, pack.size
            ),
        ));
    }
    if !has_version_token(&meta) {
        return Ok(());
    }
    let receipt = OriginReceipt::new(
        PACK_ORIGIN_NAMESPACE.to_owned(),
        pack_path.as_ref().to_owned(),
        *expected.as_bytes(),
        *expected.as_bytes(),
        pack.size,
        meta.e_tag,
        meta.version,
    );
    let bytes = serde_json::to_vec(&receipt).map_err(|error| {
        MetadataError::Internal(format!("pack-origin receipt serialize failed: {error}"))
    })?;
    store
        .put_overwrite(
            &router.pack_origin_receipt_path(&pack.pack_id),
            Bytes::from(bytes),
        )
        .await?;
    Ok(())
}

/// Verify a committed pack body or reuse proof bound to its current version.
///
/// Returns `true` when this call streamed and hashed the pack body, and
/// `false` when a durable receipt plus current object version avoided that
/// read. Backends without ETag/version support are hashed on every call.
pub async fn verify_pack_origin(
    store: &Store,
    repo_prefix: &str,
    pack: &PackManifestEntry,
) -> Result<bool> {
    let router = StoreLayout::new(store.clone(), repo_prefix.to_owned());
    if pack.pack_id != pack.content_hash {
        return Err(corrupt(
            router.pack_path(&pack.pack_id).as_ref(),
            "pack manifest identity differs from its content hash",
        ));
    }
    let expected = blake3::Hash::from_hex(&pack.content_hash).map_err(|error| {
        corrupt(
            router.pack_path(&pack.pack_id).as_ref(),
            format!("invalid pack content hash: {error}"),
        )
    })?;
    let expected_hash = *expected.as_bytes();
    let pack_path = router.pack_path(&pack.pack_id);
    let receipt_path = router.pack_origin_receipt_path(&pack.pack_id);

    for _ in 0..MAX_STABILITY_ATTEMPTS {
        let before = store.head(&pack_path).await.map_err(|error| {
            corrupt(
                pack_path.as_ref(),
                format!("committed pack is unavailable: {error}"),
            )
        })?;
        if before.size != pack.size {
            return Err(corrupt(
                pack_path.as_ref(),
                format!(
                    "committed pack has {} bytes, manifest requires {}",
                    before.size, pack.size
                ),
            ));
        }
        if read_matching_receipt(
            store,
            &receipt_path,
            &pack_path,
            expected_hash,
            pack.size,
            &before,
        )
        .await?
        {
            return Ok(false);
        }

        store
            .verify_size_and_hash(&pack_path, pack.size, &expected_hash)
            .await?;
        let after = store.head(&pack_path).await?;
        if !same_object_version(&before, &after) {
            continue;
        }

        if store.staging_write_prefix().is_none() && has_version_token(&after) {
            let receipt = OriginReceipt::new(
                PACK_ORIGIN_NAMESPACE.to_owned(),
                pack_path.as_ref().to_owned(),
                expected_hash,
                expected_hash,
                pack.size,
                after.e_tag.clone(),
                after.version.clone(),
            );
            let bytes = serde_json::to_vec(&receipt).map_err(|error| {
                MetadataError::Internal(format!("pack-origin receipt serialize failed: {error}"))
            })?;
            store
                .put_overwrite(&receipt_path, Bytes::from(bytes))
                .await?;
        }
        return Ok(true);
    }

    Err(corrupt(
        pack_path.as_ref(),
        "pack object version changed repeatedly during integrity verification",
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn entry(body: &[u8]) -> PackManifestEntry {
        let hash = blake3::hash(body).to_hex().to_string();
        PackManifestEntry {
            pack_id: hash.clone(),
            size: body.len() as u64,
            content_hash: hash,
            ref_tips: vec!["a".repeat(40)],
            object_count: 1,
        }
    }

    #[tokio::test]
    async fn receipt_avoids_second_body_hash_and_detects_same_size_replacement() {
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "repo".to_owned());
        let pack = entry(b"valid-pack-body");
        let path = router.pack_path(&pack.pack_id);
        store
            .put(&path, Bytes::from_static(b"valid-pack-body"))
            .await
            .expect("seed pack");

        assert!(
            verify_pack_origin(&store, router.repo_prefix(), &pack)
                .await
                .expect("first full verification")
        );
        assert!(
            !verify_pack_origin(&store, router.repo_prefix(), &pack)
                .await
                .expect("receipt verification")
        );

        store
            .put_overwrite(&path, Bytes::from_static(b"evil!-pack-body"))
            .await
            .expect("replace with same-size corruption");
        assert_eq!(pack.size, b"evil!-pack-body".len() as u64);
        assert!(
            verify_pack_origin(&store, router.repo_prefix(), &pack)
                .await
                .is_err()
        );
    }
}
