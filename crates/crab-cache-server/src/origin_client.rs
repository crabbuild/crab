//! Origin object store client using the cache service's own credentials.
//!
//! Wraps [`object_store::ObjectStore`] to fetch objects from the authoritative
//! cloud store (S3, GCS, Azure, or local filesystem for testing). The cache
//! service uses its own IAM role / service account; client credentials are
//! never forwarded.

use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use object_store::ObjectStoreExt;
use object_store::path::Path;
use object_store::{GetResult, ObjectStore};

use crab_storage::{UrlObjectStore, build_url_object_store};

use crate::error::{CacheServiceError, Result};

/// Non-existent key used to prove origin reachability without reading data.
pub const ORIGIN_HEALTH_PROBE_PATH: &str = ".crab/cache-service-health-probe";

/// Origin object store client with the cache service's credentials.
pub struct OriginClient {
    origin: UrlObjectStore,
}

impl OriginClient {
    /// Build an `OriginClient` from a URL string (e.g. `s3://bucket`,
    /// `gs://bucket`, `az://container`, or `file:///path`).
    ///
    /// Cloud credentials are picked up from the environment (AWS_*, GOOGLE_*,
    /// AZURE_* env vars or instance metadata) via the `object_store` crate's
    /// builder defaults.
    pub fn from_url(url: &str) -> Result<Self> {
        let origin = build_url_object_store(url).map_err(origin_config_error)?;
        Ok(Self { origin })
    }

    /// Create a stub `OriginClient` backed by a temporary local filesystem.
    ///
    /// Useful for server bootstrap and tests where no real cloud store is
    /// available. Objects can be pre-populated via the returned client's
    /// underlying store.
    pub fn stub() -> Self {
        Self::from_store(Arc::new(object_store::memory::InMemory::new()))
    }

    /// Construct from an already-built `ObjectStore` implementation.
    ///
    /// Primarily for tests that need to inject a mock or in-memory store.
    pub fn from_store(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            origin: UrlObjectStore::new(store, Path::default()),
        }
    }

    /// Fetch a full object from origin, returning a streaming [`GetResult`].
    pub async fn get(&self, path: &Path) -> Result<GetResult> {
        let origin_path = self.origin.path(path);
        self.origin
            .store()
            .get(&origin_path)
            .await
            .map_err(|e| map_error(e, &origin_path))
    }

    /// Fetch a byte range from origin.
    pub async fn get_range(&self, path: &Path, range: Range<u64>) -> Result<Bytes> {
        let origin_path = self.origin.path(path);
        self.origin
            .store()
            .get_range(&origin_path, range)
            .await
            .map_err(|e| map_error(e, &origin_path))
    }

    /// Retrieve object metadata (size, last-modified, etc.) without
    /// downloading the body.
    pub async fn head(&self, path: &Path) -> Result<object_store::ObjectMeta> {
        let origin_path = self.origin.path(path);
        self.origin
            .store()
            .head(&origin_path)
            .await
            .map_err(|e| map_error(e, &origin_path))
    }
}

pub fn origin_probe_reached_origin<T>(result: &Result<T>) -> bool {
    matches!(
        result,
        Ok(_) | Err(CacheServiceError::OriginNotFound { .. })
    )
}

fn origin_config_error(error: crab_storage::StorageError) -> CacheServiceError {
    CacheServiceError::ConfigError(format!(
        "invalid origin object-store configuration: {error}"
    ))
}

/// Map an `object_store::Error` to the appropriate `CacheServiceError`.
///
/// Connection/timeout errors map to `OriginUnreachable`; other errors map to
/// `InternalError`.
fn map_error(err: object_store::Error, path: &Path) -> CacheServiceError {
    match &err {
        // Network-level failures: the origin store cannot be reached.
        object_store::Error::Generic { source, .. } if is_connection_error(source.as_ref()) => {
            CacheServiceError::OriginUnreachable {
                reason: format!("{path}: {err}"),
            }
        }
        // Not found at origin is a legitimate 404, not an
        // unreachable-origin signal.
        object_store::Error::NotFound { .. } => CacheServiceError::OriginNotFound {
            path: path.to_string(),
        },
        // Anything else is unexpected.
        _ => CacheServiceError::InternalError(Box::new(err)),
    }
}

/// Heuristic: walk the error source chain looking for connection/timeout
/// indicators. This catches hyper, reqwest, and std::io connection errors
/// regardless of which cloud backend produced them.
fn is_connection_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let msg = err.to_string().to_lowercase();
    if msg.contains("connect")
        || msg.contains("timeout")
        || msg.contains("timed out")
        || msg.contains("connection refused")
        || msg.contains("dns")
        || msg.contains("unreachable")
    {
        return true;
    }
    // Walk the source chain.
    if let Some(source) = err.source() {
        return is_connection_error(source);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_probe_reaches_origin_on_success_or_expected_missing_probe_object() {
        assert!(origin_probe_reached_origin(&Ok(())));
        assert!(origin_probe_reached_origin::<()>(&Err(
            CacheServiceError::OriginNotFound {
                path: ORIGIN_HEALTH_PROBE_PATH.to_string(),
            },
        )));
    }

    #[test]
    fn origin_probe_does_not_reach_origin_on_connectivity_or_internal_error() {
        assert!(!origin_probe_reached_origin::<()>(&Err(
            CacheServiceError::OriginUnreachable {
                reason: "connection refused".to_string(),
            },
        )));
        assert!(!origin_probe_reached_origin::<()>(&Err(
            CacheServiceError::InternalError(Box::new(std::io::Error::other(
                "origin returned 503",
            ))),
        )));
    }

    #[tokio::test]
    async fn origin_client_applies_url_prefix_to_origin_requests() {
        let store = Arc::new(object_store::memory::InMemory::new());
        store
            .put(
                &Path::from("base/prefix/repo/object"),
                Bytes::from_static(b"data").into(),
            )
            .await
            .expect("in-memory origin put should succeed");
        let client = OriginClient {
            origin: UrlObjectStore::new(store, Path::from("base/prefix")),
        };

        let meta = client
            .head(&Path::from("repo/object"))
            .await
            .expect("origin URL prefix should be applied before lookup");

        assert_eq!(meta.location.as_ref(), "base/prefix/repo/object");
    }
}
