//! Origin-only integrity checks over a caller-pinned recipe.

use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use crab_xet::shard::MDBFileInfo;
use crab_xet::xorb::{format::MAX_XORB_SIZE, parser::XorbParser};
use tokio_util::sync::CancellationToken;

use crate::{ReadError, ReadStoreLayout, Result};

/// Verify every byte of a pinned recipe against origin and the pointer hash/size.
///
/// The caller must supply the authoritative provider store (not a cache-aware
/// adapter), verify recipe ownership and protect its snapshot lifetime.
/// Reads bypass all caches and retain at most one serialized xorb and one decoded
/// chunk; no working-tree data or remote objects are written.
pub async fn verify_origin_recipe(
    layout: &ReadStoreLayout,
    pointer: &Pointer,
    recipe: &MDBFileInfo,
    cancel: &CancellationToken,
) -> Result<u64> {
    let file_hash = MerkleHash::from(pointer.file_hash);
    let corrupt = |reason: &str| ReadError::CorruptObject {
        path: file_hash.hex(),
        reason: reason.to_owned(),
    };
    if cancel.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    if recipe.metadata.file_hash != file_hash {
        return Err(corrupt("recipe belongs to a different file"));
    }
    if u64::from(recipe.metadata.num_entries) != recipe.segments.len() as u64 {
        return Err(corrupt("recipe segment count differs from its header"));
    }
    let expected_bytes = recipe.segments.iter().try_fold(0u64, |total, segment| {
        total
            .checked_add(u64::from(segment.unpacked_segment_bytes))
            .ok_or_else(|| corrupt("recipe byte count overflow"))
    })?;
    if expected_bytes != pointer.size {
        return Err(corrupt("recipe length differs from pointer length"));
    }

    let mut hasher = blake3::Hasher::new();
    let mut previous: Option<XorbParser> = None;
    for segment in &recipe.segments {
        if cancel.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        let body = if previous
            .as_ref()
            .is_some_and(|xorb| xorb.hash() == segment.xorb_hash)
        {
            None
        } else {
            // Drop the previous body before allocating the next one. In particular,
            // a shared content cache must never conceal an unreadable origin here.
            previous = None;
            let path = layout.xorb_path(&segment.xorb_hash);
            let read = layout
                .store()
                .get_with_etag_bounded(&path, MAX_XORB_SIZE as u64);
            Some(tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ReadError::Cancelled),
                result = read => result?.0,
            })
        };
        let segment = segment.clone();
        let worker_cancel = cancel.clone();
        let (parser, updated) = tokio::task::spawn_blocking(move || {
            let parser = match (body, previous) {
                (Some(body), _) => {
                    let parser = XorbParser::parse(body)?;
                    if parser.hash() != segment.xorb_hash {
                        return Err(ReadError::HashMismatch {
                            requested: segment.xorb_hash.hex(),
                            actual: parser.hash().hex(),
                        });
                    }
                    parser.verify_payload_digest()?;
                    parser
                }
                (None, Some(parser)) => parser,
                (None, None) => return Err(ReadError::internal("missing origin xorb body")),
            };
            if segment.chunk_index_start > segment.chunk_index_end
                || segment.chunk_index_end > parser.num_chunks()
            {
                return Err(ReadError::CorruptObject {
                    path: segment.xorb_hash.hex(),
                    reason: "recipe chunk range is reversed or outside xorb bounds".to_owned(),
                });
            }
            let mut bytes = 0u64;
            for index in segment.chunk_index_start..segment.chunk_index_end {
                if worker_cancel.is_cancelled() {
                    return Err(ReadError::Cancelled);
                }
                let chunk = parser.get_chunk(index)?;
                bytes += chunk.data.len() as u64;
                hasher.update(&chunk.data);
            }
            if bytes != u64::from(segment.unpacked_segment_bytes) {
                return Err(ReadError::CorruptObject {
                    path: segment.xorb_hash.hex(),
                    reason: "decoded recipe segment length differs from declared length".to_owned(),
                });
            }
            Ok::<_, ReadError>((parser, hasher))
        })
        .await
        .map_err(|error| ReadError::Io(std::io::Error::other(error)))??;
        previous = Some(parser);
        hasher = updated;
    }
    if cancel.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    let actual = hasher.finalize();
    if actual.as_bytes() != &pointer.file_hash {
        return Err(ReadError::HashMismatch {
            requested: file_hash.hex(),
            actual: actual.to_hex().to_string(),
        });
    }
    Ok(expected_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use bytes::Bytes;
    use crab_storage::{StorageError, Store};
    use crab_xet::shard::{FileDataSequenceEntry, FileDataSequenceHeader};
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;

    async fn fixture() -> (ReadStoreLayout, Pointer, MDBFileInfo, Bytes) {
        let layout = ReadStoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "origin-proof".to_owned(),
        );
        let mut builder = XorbBuilder::new();
        for data in [b"alpha".as_slice(), b"beta", b"gamma"] {
            builder
                .push(&Chunk::new(Bytes::copy_from_slice(data)), RunId(0))
                .unwrap();
        }
        let xorb = builder.finalize().unwrap().pop().unwrap();
        layout
            .store()
            .put(&layout.xorb_path(&xorb.hash), xorb.bytes.clone())
            .await
            .unwrap();
        let pointer = Pointer {
            file_hash: blake3::hash(b"gammaalphabeta").into(),
            size: 14,
            shard_hint: None,
        };
        let recipe = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(
                MerkleHash::from(pointer.file_hash),
                2,
                false,
                false,
            ),
            segments: vec![
                FileDataSequenceEntry::new(xorb.hash, 5, 2, 3),
                FileDataSequenceEntry::new(xorb.hash, 9, 0, 2),
            ],
            verification: vec![],
            metadata_ext: None,
        };
        (layout, pointer, recipe, xorb.bytes)
    }

    #[tokio::test]
    async fn origin_recipe_verifies_reordered_segments_and_exact_bytes() {
        let (layout, pointer, recipe, _) = fixture().await;
        assert_eq!(
            verify_origin_recipe(&layout, &pointer, &recipe, &CancellationToken::new())
                .await
                .unwrap(),
            14
        );
    }

    #[tokio::test]
    async fn origin_recipe_does_not_accept_a_healthy_cache_for_corrupt_origin() {
        let (layout, pointer, recipe, body) = fixture().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = crab_cache::LocalCache::new(cache_dir.path().join("cache"));
        let hash = recipe.segments[0].xorb_hash;
        let key = crab_cache::CacheKey::Xorb(hash);
        cache.put_bytes(&key, body.clone()).await.unwrap();
        assert!(cache.contains_verified(&key).await);
        let mut corrupt = body.to_vec();
        corrupt[0] ^= 1;
        // Simulate out-of-band damage; normal immutable PUT correctly refuses
        // to overwrite an existing content-addressed object.
        layout
            .store()
            .delete(&layout.xorb_path(&hash))
            .await
            .unwrap();
        layout
            .store()
            .put(&layout.xorb_path(&hash), Bytes::from(corrupt))
            .await
            .unwrap();
        assert!(matches!(
            verify_origin_recipe(&layout, &pointer, &recipe, &CancellationToken::new()).await,
            Err(ReadError::Xet(
                crab_xet::error::XetError::CorruptObject { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn origin_recipe_missing_object_retains_storage_error() {
        let (layout, pointer, recipe, _) = fixture().await;
        layout
            .store()
            .delete(&layout.xorb_path(&recipe.segments[0].xorb_hash))
            .await
            .unwrap();
        assert!(matches!(
            verify_origin_recipe(&layout, &pointer, &recipe, &CancellationToken::new()).await,
            Err(ReadError::Storage(StorageError::NotFound { .. }))
        ));
    }

    #[tokio::test]
    async fn origin_recipe_rejects_wrong_whole_file_hash() {
        let (layout, mut pointer, mut recipe, _) = fixture().await;
        pointer.file_hash = [42; 32];
        recipe.metadata.file_hash = MerkleHash::from(pointer.file_hash);
        assert!(matches!(
            verify_origin_recipe(&layout, &pointer, &recipe, &CancellationToken::new()).await,
            Err(ReadError::HashMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn origin_recipe_rejects_invalid_segment_contracts() {
        for case in [
            "owner",
            "pointer_size",
            "segment_size",
            "range",
            "empty_range",
            "segment_count",
        ] {
            let (layout, mut pointer, mut recipe, _) = fixture().await;
            match case {
                "owner" => recipe.metadata.file_hash = MerkleHash::from([42; 32]),
                "pointer_size" => pointer.size += 1,
                "segment_size" => {
                    recipe.segments[0].unpacked_segment_bytes += 1;
                    recipe.segments[1].unpacked_segment_bytes -= 1;
                }
                "range" => recipe.segments[0].chunk_index_end = 4,
                "empty_range" => recipe.segments[0].chunk_index_end = 2,
                "segment_count" => recipe.metadata.num_entries += 1,
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    verify_origin_recipe(&layout, &pointer, &recipe, &CancellationToken::new())
                        .await,
                    Err(ReadError::CorruptObject { .. })
                ),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn origin_recipe_empty_file_requires_the_empty_hash() {
        let (layout, mut pointer, mut recipe, _) = fixture().await;
        pointer.file_hash = blake3::hash(b"").into();
        pointer.size = 0;
        recipe.metadata.file_hash = MerkleHash::from(pointer.file_hash);
        recipe.metadata.num_entries = 0;
        recipe.segments.clear();
        assert_eq!(
            verify_origin_recipe(&layout, &pointer, &recipe, &CancellationToken::new())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn origin_recipe_cancelled_check_cannot_pass() {
        let (layout, pointer, recipe, _) = fixture().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            verify_origin_recipe(&layout, &pointer, &recipe, &cancel).await,
            Err(ReadError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn origin_recipe_cancellation_interrupts_pending_origin_read() {
        use object_store::throttle::{ThrottleConfig, ThrottledStore};
        use std::time::Duration;

        let (layout, pointer, recipe, _) = fixture().await;
        let throttled = Store::new(Arc::new(ThrottledStore::new(
            Arc::clone(layout.store().inner()),
            ThrottleConfig {
                wait_get_per_call: Duration::from_secs(10),
                ..ThrottleConfig::default()
            },
        )));
        let layout = ReadStoreLayout::new(throttled, layout.repo_prefix().to_owned());
        let cancel = CancellationToken::new();
        let verification = verify_origin_recipe(&layout, &pointer, &recipe, &cancel);
        tokio::pin!(verification);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut verification)
                .await
                .is_err()
        );
        cancel.cancel();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), &mut verification)
                .await
                .unwrap(),
            Err(ReadError::Cancelled)
        ));
    }
}
