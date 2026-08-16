//! Source-level and per-blob Git LFS detection for `crab import`.
//!
//! Two guards live here:
//!
//! 1. [`detect_lfs_source`] inspects the source prefix for a
//!    `.gitattributes` file and classifies the bucket layout. A
//!    `filter=lfs` line anywhere in that file means the source is
//!    an LFS-format tree (pointer blobs + an LFS-store sibling),
//!    and the orchestrator applies the selected import policy:
//!    fail, resolve, or skip. A missing or non-LFS `.gitattributes`
//!    returns [`LfsDetection::Plain`] and the import proceeds.
//! 2. Per-blob detection lives in `ingest::process_entry`, which
//!    calls [`crab_git::pointer_detect::classify`]
//!    on small-enough objects to catch stray LFS pointer blobs
//!    even in otherwise plain buckets. That path flags the entry
//!    `Skipped { reason: LfsPointer }` and surfaces a count in the
//!    summary.
//!
//! Source-level detection only decides policy. Per-blob handling
//! either rehydrates through a companion LFS object root or records
//! the pointer as skipped.

use object_store::path::Path as ObjectPath;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::import::ingest::ResolvedStore;

/// Outcome of inspecting the source prefix for LFS-format markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LfsDetection {
    /// No `.gitattributes` at the source prefix, or one that does
    /// not mention `filter=lfs`. Import proceeds normally; per-blob
    /// LFS detection still applies during ingest.
    Plain,
    /// `.gitattributes` is present and has at least one line
    /// containing `filter=lfs`. When `store_location` is `Some`,
    /// the companion LFS object store was located and can be used
    /// for pointer rehydration during import.
    LfsFormat {
        /// Resolved location of the companion LFS object store, if discovered.
        store_location: Option<String>,
    },
}

/// Inspect the source prefix for LFS-format indicators.
///
/// Issues a single GET at `{prefix}/.gitattributes`:
///
/// - Absent (mapped to [`CrabError::NotFound`]): return
///   [`LfsDetection::Plain`]. Flat buckets rarely carry a
///   `.gitattributes` and we treat that as the common case.
/// - Present: decode as UTF-8 (lossy on invalid bytes so a
///   mis-encoded file can't panic) and walk the lines. A line
///   containing `filter=lfs` anywhere — attribute position or
///   value — flips the verdict to [`LfsDetection::LfsFormat`].
///   Comments (`# …`) still count: a user committing a
///   `# filter=lfs` line in a `.gitattributes` is either lying
///   to themselves or aware of what they have; either way we
///   refuse and let them sort it out.
/// - Any other error (forbidden, transient network, decode
///   refused by the backend, etc.) propagates as-is.
///
/// # Cancellation
///
/// Checks the cancellation token once before the GET so a late
/// cancel doesn't kick off the network call. Mid-GET cancellation
/// surfaces as whatever the object store returns for a dropped
/// request, typically [`CrabError::NetworkTransient`].
pub async fn detect_lfs_source(
    source: &ResolvedStore,
    cancel: &CancellationToken,
) -> Result<LfsDetection> {
    check_cancelled(cancel)?;

    let path = gitattributes_path(&source.prefix);
    debug!(
        prefix = %source.prefix,
        path = %path,
        "lfs_guard: probing .gitattributes at source prefix"
    );

    // Use get_with_etag because Store lacks a plain `get` method —
    // we discard the etag. The retry layer wraps the GET so
    // transient failures get a second shot before surfacing.
    let body = match source.store.get_with_etag(&path).await {
        Ok((bytes, _etag)) => bytes,
        Err(CrabError::NotFound { .. }) => {
            debug!(
                prefix = %source.prefix,
                "lfs_guard: no .gitattributes at source prefix; classifying as Plain"
            );
            return Ok(LfsDetection::Plain);
        }
        Err(other) => return Err(other),
    };

    // `from_utf8_lossy` keeps us panic-free on weird encodings.
    // `.gitattributes` is ASCII in practice; lossy decoding just
    // keeps the line-scan honest for the pathological case.
    let text = String::from_utf8_lossy(&body);
    for line in text.lines() {
        if line.contains("filter=lfs") {
            debug!(
                prefix = %source.prefix,
                "lfs_guard: .gitattributes declares filter=lfs; classifying as LfsFormat"
            );
            return Ok(LfsDetection::LfsFormat {
                store_location: None,
            });
        }
    }

    debug!(
        prefix = %source.prefix,
        "lfs_guard: .gitattributes present but no filter=lfs; classifying as Plain"
    );
    Ok(LfsDetection::Plain)
}

/// Build the object-store path for the source prefix's
/// `.gitattributes`. Empty prefixes map to the bucket root;
/// non-empty prefixes get the leaf joined with `/`.
fn gitattributes_path(prefix: &str) -> ObjectPath {
    if prefix.is_empty() {
        ObjectPath::from(".gitattributes")
    } else {
        ObjectPath::from(format!("{prefix}/.gitattributes"))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

    use crate::storage::store::{BucketIdentity, Store};

    fn resolved(inner: Arc<dyn ObjectStore>, prefix: &str) -> ResolvedStore {
        ResolvedStore {
            store: Store::new(inner),
            bucket: BucketIdentity::local_unset(),
            prefix: prefix.to_owned(),
        }
    }

    async fn seed(store: &Arc<dyn ObjectStore>, key: &str, body: &[u8]) {
        store
            .put(
                &ObjectPath::from(key.to_owned()),
                PutPayload::from(Bytes::from(body.to_vec())),
            )
            .await
            .expect("seed");
    }

    // ── path helper ─────────────────────────────────────────────

    #[test]
    fn gitattributes_path_handles_empty_and_nested_prefixes() {
        assert_eq!(gitattributes_path("").as_ref(), ".gitattributes");
        assert_eq!(
            gitattributes_path("data/v2").as_ref(),
            "data/v2/.gitattributes"
        );
    }

    // ── 10.3: .gitattributes with filter=lfs → LfsFormat ────────

    #[tokio::test]
    async fn filter_lfs_line_is_detected_as_lfs_format() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        seed(
            &inner,
            "src/.gitattributes",
            b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .await;
        let src = resolved(Arc::clone(&inner), "src");
        let cancel = CancellationToken::new();

        let got = detect_lfs_source(&src, &cancel).await.unwrap();
        assert!(matches!(got, LfsDetection::LfsFormat { .. }));
    }

    #[tokio::test]
    async fn filter_lfs_line_among_others_is_still_detected() {
        // Mixed attributes: a plain binary line plus one LFS line
        // — the LFS line is enough to refuse.
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        seed(
            &inner,
            ".gitattributes",
            b"*.md text\n\
              *.png binary\n\
              *.safetensors filter=lfs diff=lfs merge=lfs -text\n",
        )
        .await;
        let src = resolved(Arc::clone(&inner), "");
        let cancel = CancellationToken::new();

        assert!(matches!(
            detect_lfs_source(&src, &cancel).await.unwrap(),
            LfsDetection::LfsFormat { .. }
        ));
    }

    // ── 10.3: plain .gitattributes → Plain ──────────────────────

    #[tokio::test]
    async fn plain_gitattributes_is_classified_plain() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        seed(&inner, "src/.gitattributes", b"*.md text\n*.png binary\n").await;
        let src = resolved(Arc::clone(&inner), "src");
        let cancel = CancellationToken::new();

        assert_eq!(
            detect_lfs_source(&src, &cancel).await.unwrap(),
            LfsDetection::Plain
        );
    }

    // ── 10.3: no .gitattributes → Plain ─────────────────────────

    #[tokio::test]
    async fn missing_gitattributes_is_classified_plain() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        // Seed an unrelated object so the store isn't empty.
        seed(&inner, "src/data.bin", b"hello").await;
        let src = resolved(Arc::clone(&inner), "src");
        let cancel = CancellationToken::new();

        assert_eq!(
            detect_lfs_source(&src, &cancel).await.unwrap(),
            LfsDetection::Plain
        );
    }

    #[tokio::test]
    async fn empty_bucket_is_classified_plain() {
        // No prefix, no objects at all — the NotFound path at the
        // bucket root still lands in `Plain`.
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let src = resolved(Arc::clone(&inner), "");
        let cancel = CancellationToken::new();

        assert_eq!(
            detect_lfs_source(&src, &cancel).await.unwrap(),
            LfsDetection::Plain
        );
    }

    #[tokio::test]
    async fn cancellation_short_circuits_before_get() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let src = resolved(Arc::clone(&inner), "");
        let cancel = CancellationToken::new();
        cancel.cancel();

        let err = detect_lfs_source(&src, &cancel).await.unwrap_err();
        assert!(matches!(err, CrabError::Cancelled), "got {err:?}");
    }
}
