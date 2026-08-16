//! Storage-class–aware HEAD helper for the tiering subsystem.
//!
//! [`head_with_class`] returns [`HeadMeta`] enriched with the
//! provider-native [`StorageClass`]. When a tier feature is enabled
//! (`tier-s3`, `tier-gcs`, `tier-azure`), the implementation
//! delegates to the provider SDK for the real class. When no tier
//! feature is compiled in, the stub returns
//! [`StorageClass::Unknown`] so callers (notably `cmd/hydrate`)
//! treat the object as warm — preserving backward compatibility.
//!
//! # Why not `object_store`?
//!
//! The `object_store` crate exposes `Attribute::StorageClass` as a
//! *write-side* input (for setting the class on PUT) but does not
//! parse it from HEAD/GET responses — the `parse_attributes!` macro
//! in `client/get.rs` covers only `Cache-Control`,
//! `Content-Disposition`, `Content-Encoding`, `Content-Language`,
//! and `Content-Type`. Bringing storage-class parsing to
//! `object_store` would be a clean upstream contribution, but until
//! that lands we dispatch to the provider SDKs that already expose
//! the field.
//!
//! # Per-provider implementations
//!
//! All three providers follow the same shape:
//!
//! 1. Construct a provider SDK client from ambient env credentials
//!    (matching the existing `S3LifecycleProvider::new` pattern).
//! 2. Issue the SDK's metadata call for the object:
//!    - **S3**: `head_object` → `StorageClass` header.
//!    - **GCS**: `get_object` → `Object::storage_class` field.
//!    - **Azure**: `get_properties` → `BlobProperties::access_tier`.
//! 3. Map the provider-native class string to our neutral
//!    [`StorageClass`] via
//!    [`StorageClass::from_provider_str`].
//!
//! # Fallback policy
//!
//! Any of the following degrade to `HeadMeta { class: Unknown }`
//! with a `warn!` line, never a hard error:
//!
//! - Missing ambient credentials (no `AWS_REGION`,
//!   `AZURE_STORAGE_ACCOUNT`, etc.).
//! - Client construction failure.
//! - SDK call error.
//! - Response field absent (e.g. S3 omits the header for Standard-
//!   class objects — that case is mapped to the explicit provider
//!   default rather than `Unknown`).
//! - Unrecognized class string.
//!
//! The contract class-aware GC cares about: **never crash because a
//! class probe failed; just be pessimistic**. An `Unknown` answer
//! tells downstream callers to treat the object as warm (no
//! aggressive archive transitions), which is always safe.

use object_store::path::Path;
use tracing::warn;

use crate::core::error::Result;
#[cfg(any(feature = "tier-s3", feature = "tier-gcs", feature = "tier-azure"))]
use crate::git::url::Cloud;
use crate::storage::Store;
use crate::tier::StorageClass;
use crate::tier::provider::HeadMeta;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Fetch object metadata including the provider-native storage class.
///
/// When no tier feature is enabled, returns a stubbed `HeadMeta` with
/// `class: StorageClass::Unknown` (treated as warm by the hydrate
/// pipeline). When a tier feature is on, delegates to the provider
/// SDK for the real class.
pub async fn head_with_class(store: &Store, path: &Path) -> Result<HeadMeta> {
    #[cfg(any(feature = "tier-s3", feature = "tier-gcs", feature = "tier-azure"))]
    {
        head_with_class_provider(store, path).await
    }

    #[cfg(not(any(feature = "tier-s3", feature = "tier-gcs", feature = "tier-azure")))]
    {
        let _ = (store, path);
        Ok(HeadMeta {
            class: StorageClass::Unknown,
        })
    }
}

/// Dispatch the class probe to the provider that backs `store`.
///
/// Uses [`Store::bucket_identity`] to decide which provider SDK to
/// hit. `Cloud::Local` and clouds whose provider-feature isn't
/// compiled in route to the stub path, where we confirm existence
/// via `object_store::head` and return `Unknown`.
#[cfg(any(feature = "tier-s3", feature = "tier-gcs", feature = "tier-azure"))]
async fn head_with_class_provider(store: &Store, path: &Path) -> Result<HeadMeta> {
    let identity = store.bucket_identity();

    match identity.cloud {
        #[cfg(feature = "tier-s3")]
        Cloud::S3 => head_with_class_s3(&identity.container, path).await,

        #[cfg(feature = "tier-gcs")]
        Cloud::Gcs => head_with_class_gcs(&identity.container, path).await,

        #[cfg(feature = "tier-azure")]
        Cloud::Azure => head_with_class_azure(&identity.host, &identity.container, path).await,

        _ => head_with_class_stub(store, path).await,
    }
}

/// Fallback that confirms the object exists via the generic
/// `object_store` HEAD but cannot resolve its storage class.
///
/// Used when (a) no provider-specific SDK is compiled in for this
/// cloud, or (b) the store identity is `Local`. Returning `Unknown`
/// keeps the tiering pipeline pessimistic-but-correct (treats
/// everything as warm, so no aggressive archive transitions based
/// on missing data).
#[cfg(any(feature = "tier-s3", feature = "tier-gcs", feature = "tier-azure"))]
async fn head_with_class_stub(store: &Store, path: &Path) -> Result<HeadMeta> {
    let _meta = store.head(path).await?;
    Ok(HeadMeta {
        class: StorageClass::Unknown,
    })
}

// ---------------------------------------------------------------------------
// S3 delegation
// ---------------------------------------------------------------------------

/// Real S3 delegation: constructs an `aws-sdk-s3` client from the
/// ambient AWS credential chain and issues a `HeadObject` call,
/// then maps the response's `StorageClass` header to our enum.
///
/// S3 omits the `StorageClass` header entirely when the object is
/// in Standard — treat `None` as explicit `S3Standard` rather than
/// `Unknown` so class-aware GC can reason about the common case.
#[cfg(feature = "tier-s3")]
async fn head_with_class_s3(bucket: &str, path: &Path) -> Result<HeadMeta> {
    use crate::tier::provider::Provider;

    let key = path.as_ref();

    let client = match build_s3_client().await {
        Ok(c) => c,
        Err(e) => {
            warn!(
                bucket = %bucket,
                key = %key,
                error = %e,
                "S3 storage-class probe: client construction failed, \
                 falling back to Unknown class"
            );
            return Ok(HeadMeta {
                class: StorageClass::Unknown,
            });
        }
    };

    let resp = match client
        .head_object()
        .bucket(bucket.to_owned())
        .key(key.to_owned())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(
                bucket = %bucket,
                key = %key,
                error = %e,
                "S3 HeadObject failed for storage-class probe; \
                 falling back to Unknown class"
            );
            return Ok(HeadMeta {
                class: StorageClass::Unknown,
            });
        }
    };

    let class = match resp.storage_class() {
        None => StorageClass::S3Standard,
        Some(c) => {
            let parsed = StorageClass::from_provider_str(&Provider::S3, c.as_str());
            if parsed == StorageClass::Unknown {
                warn!(
                    bucket = %bucket,
                    key = %key,
                    raw_class = c.as_str(),
                    "S3 HeadObject returned an unrecognized storage class; \
                     mapping to Unknown"
                );
            }
            parsed
        }
    };

    Ok(HeadMeta { class })
}

/// Build an `aws-sdk-s3::Client` for a HEAD probe using the ambient
/// region and the SDK's default credential chain.
#[cfg(feature = "tier-s3")]
async fn build_s3_client() -> Result<aws_sdk_s3::Client> {
    use crate::core::error::CrabError;
    use aws_sdk_s3::config::{BehaviorVersion, Region};

    let region_str = std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .map_err(|_| CrabError::Configuration {
            key: "AWS_REGION".into(),
            origin: "environment".into(),
        })?;

    let config = aws_sdk_s3::config::Builder::new()
        .region(Region::new(region_str))
        .behavior_version(BehaviorVersion::latest())
        .build();
    Ok(aws_sdk_s3::Client::from_conf(config))
}

// ---------------------------------------------------------------------------
// GCS delegation
// ---------------------------------------------------------------------------

/// Real GCS delegation: constructs a `google-cloud-storage` client
/// using the default GCP credential chain and issues a `get_object`
/// call to read `Object::storage_class`.
///
/// Unlike S3, GCS always populates `storage_class` — there is no
/// "absent header means Standard" case. A `None` here means the
/// SDK couldn't parse the response and degrades to `Unknown`.
#[cfg(feature = "tier-gcs")]
async fn head_with_class_gcs(bucket: &str, path: &Path) -> Result<HeadMeta> {
    use crate::tier::provider::Provider;
    use google_cloud_storage::http::objects::get::GetObjectRequest;

    let key = path.as_ref();

    let client = match build_gcs_client().await {
        Ok(c) => c,
        Err(e) => {
            warn!(
                bucket = %bucket,
                key = %key,
                error = %e,
                "GCS storage-class probe: client construction failed, \
                 falling back to Unknown class"
            );
            return Ok(HeadMeta {
                class: StorageClass::Unknown,
            });
        }
    };

    let req = GetObjectRequest {
        bucket: bucket.to_owned(),
        object: key.to_owned(),
        ..Default::default()
    };

    let obj = match client.get_object(&req).await {
        Ok(o) => o,
        Err(e) => {
            warn!(
                bucket = %bucket,
                key = %key,
                error = %e,
                "GCS get_object failed for storage-class probe; \
                 falling back to Unknown class"
            );
            return Ok(HeadMeta {
                class: StorageClass::Unknown,
            });
        }
    };

    let class = match obj.storage_class.as_deref() {
        None => {
            warn!(
                bucket = %bucket,
                key = %key,
                "GCS get_object returned an object without a storage_class field; \
                 falling back to Unknown class"
            );
            StorageClass::Unknown
        }
        Some(raw) => {
            let parsed = StorageClass::from_provider_str(&Provider::Gcs, raw);
            if parsed == StorageClass::Unknown {
                warn!(
                    bucket = %bucket,
                    key = %key,
                    raw_class = raw,
                    "GCS get_object returned an unrecognized storage class; \
                     mapping to Unknown"
                );
            }
            parsed
        }
    };

    Ok(HeadMeta { class })
}

/// Build a `google-cloud-storage::Client` using the default GCP
/// credential chain (`GOOGLE_APPLICATION_CREDENTIALS`, GCE metadata,
/// or gcloud CLI creds).
#[cfg(feature = "tier-gcs")]
async fn build_gcs_client() -> Result<google_cloud_storage::client::Client> {
    use crate::core::error::CrabError;

    let config = google_cloud_storage::client::ClientConfig::default()
        .with_auth()
        .await
        .map_err(|e| {
            CrabError::Internal(format!(
                "GCS storage-class probe: credential load failed: {e}"
            ))
        })?;
    Ok(google_cloud_storage::client::Client::new(config))
}

// ---------------------------------------------------------------------------
// Azure delegation
// ---------------------------------------------------------------------------

/// Real Azure delegation: constructs a `BlobClient` using the
/// storage account + access key from env and calls `get_properties`
/// to read `BlobProperties::access_tier`.
///
/// The `Store::bucket_identity` fields map as:
/// - `identity.host` → storage account name.
/// - `identity.container` → blob container name.
///
/// `path` is the blob name within that container.
///
/// This path is functional only when the deployment supplies
/// ambient Azure credentials via env vars. A full OIDC / Entra ID
/// integration lands with the Azure provider's
/// [`AzureLifecycleProvider`] path and is tracked there.
#[cfg(feature = "tier-azure")]
async fn head_with_class_azure(account: &str, container: &str, path: &Path) -> Result<HeadMeta> {
    use crate::tier::provider::Provider;

    let blob_name = path.as_ref();

    let client = match build_azure_blob_client(account, container, blob_name) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                account = %account,
                container = %container,
                blob = %blob_name,
                error = %e,
                "Azure storage-class probe: client construction failed, \
                 falling back to Unknown class"
            );
            return Ok(HeadMeta {
                class: StorageClass::Unknown,
            });
        }
    };

    let props = match client.get_properties().await {
        Ok(p) => p,
        Err(e) => {
            warn!(
                account = %account,
                container = %container,
                blob = %blob_name,
                error = %e,
                "Azure get_properties failed for storage-class probe; \
                 falling back to Unknown class"
            );
            return Ok(HeadMeta {
                class: StorageClass::Unknown,
            });
        }
    };

    // `access_tier` is `Option<AccessTier>`. Azure omits the tier
    // for blobs in the implicit default, which is storage-account
    // dependent — we don't have a safe "default tier" to assume
    // like S3's Standard, so `None` degrades to `Unknown`.
    let class = match props.blob.properties.access_tier {
        None => {
            warn!(
                account = %account,
                container = %container,
                blob = %blob_name,
                "Azure get_properties returned no access_tier; \
                 falling back to Unknown class"
            );
            StorageClass::Unknown
        }
        Some(tier) => {
            let raw = tier.as_ref();
            let parsed = StorageClass::from_provider_str(&Provider::Azure, raw);
            if parsed == StorageClass::Unknown {
                warn!(
                    account = %account,
                    container = %container,
                    blob = %blob_name,
                    raw_class = raw,
                    "Azure get_properties returned an unrecognized access tier; \
                     mapping to Unknown"
                );
            }
            parsed
        }
    };

    Ok(HeadMeta { class })
}

/// Build an Azure `BlobClient` from `AZURE_STORAGE_ACCOUNT` and
/// `AZURE_STORAGE_ACCESS_KEY` env vars.
///
/// The storage-class probe uses shared-key auth deliberately:
/// - The operation is read-only and local-to-crab (no user-
///   visible credential surface).
/// - Full AAD / OIDC integration is spec-level work tracked under
///   `crab-enterprise-auth` Azure Entra paths; using shared-key
///   here unblocks class-aware GC on Azure without forcing that
///   cross-cutting change first.
///
/// Deployments that use OIDC exclusively will see the probe
/// degrade cleanly to `Unknown` with a warn line.
#[cfg(feature = "tier-azure")]
fn build_azure_blob_client(
    account: &str,
    container: &str,
    blob_name: &str,
) -> Result<azure_storage_blobs::prelude::BlobClient> {
    use azure_storage::StorageCredentials;
    use azure_storage_blobs::prelude::ClientBuilder;

    use crate::core::error::CrabError;

    // Allow the access-key env var to override the account name so
    // deployments that pass creds explicitly work even if the
    // BucketIdentity reports a different host.
    let key = std::env::var("AZURE_STORAGE_ACCESS_KEY").map_err(|_| CrabError::Configuration {
        key: "AZURE_STORAGE_ACCESS_KEY".into(),
        origin: "environment".into(),
    })?;

    let credentials = StorageCredentials::access_key(account.to_owned(), key);
    let service_client = ClientBuilder::new(account.to_owned(), credentials);
    let blob_client = service_client.blob_client(container.to_owned(), blob_name.to_owned());
    Ok(blob_client)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use object_store::ObjectStoreExt;

    /// Stores built without an explicit identity default to `Local`,
    /// which routes to the stub path and returns `Unknown`. This
    /// mirrors the old behavior and keeps local filesystem / InMemory
    /// tests deterministic regardless of which tier features are on.
    #[tokio::test]
    async fn local_identity_returns_unknown_class() {
        let mem = std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(mem.clone());
        let path = Path::from("test/object");
        let data = bytes::Bytes::from_static(b"hello");
        mem.put(&path, data.into()).await.unwrap();

        let meta = head_with_class(&store, &path).await.unwrap();
        assert_eq!(meta.class, StorageClass::Unknown);
    }

    #[tokio::test]
    async fn missing_object_returns_error() {
        let mem = std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(mem);
        let path = Path::from("does/not/exist");

        let result = head_with_class(&store, &path).await;
        assert!(result.is_err());
    }

    /// When the store's identity reports S3 but `AWS_REGION` is unset,
    /// the S3 path must degrade to `Unknown` rather than propagate a
    /// configuration error. Class-aware GC needs to keep working
    /// pessimistically when the region can't be resolved, not hard-fail.
    #[cfg(feature = "tier-s3")]
    #[tokio::test]
    async fn s3_client_build_missing_region_falls_back_to_unknown() {
        use crate::git::url::Cloud;
        use crate::storage::store::BucketIdentity;

        // Clear both region vars. See SAFETY comment below for the
        // justification of `unsafe` on env mutation in tests.
        let prior_region = std::env::var_os("AWS_REGION");
        let prior_default = std::env::var_os("AWS_DEFAULT_REGION");
        // SAFETY: `remove_var` is unsafe because concurrent readers
        // elsewhere in the process could see a torn value. Tokio's
        // default test runtime is single-threaded and no other task
        // reads AWS_REGION during this test.
        unsafe {
            std::env::remove_var("AWS_REGION");
            std::env::remove_var("AWS_DEFAULT_REGION");
        }

        let mem = std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(mem.clone()).with_bucket_identity(BucketIdentity::new(
            Cloud::S3,
            "test-bucket",
            "test-bucket",
        ));

        let result = head_with_class(&store, &Path::from("any/key")).await;
        assert!(result.is_ok(), "must fall back to Ok rather than error");
        assert_eq!(result.unwrap().class, StorageClass::Unknown);

        // SAFETY: restoring prior values is as safe as the initial
        // removal — same concurrency justification.
        unsafe {
            if let Some(v) = prior_region {
                std::env::set_var("AWS_REGION", v);
            }
            if let Some(v) = prior_default {
                std::env::set_var("AWS_DEFAULT_REGION", v);
            }
        }
    }

    /// Same fallback policy for GCS: without credentials the probe
    /// should degrade to `Unknown`, not error. We exercise it by
    /// clearing `GOOGLE_APPLICATION_CREDENTIALS` — the SDK's default
    /// chain will then fail during client construction.
    #[cfg(feature = "tier-gcs")]
    #[tokio::test]
    async fn gcs_client_build_missing_credentials_falls_back_to_unknown() {
        use crate::git::url::Cloud;
        use crate::storage::store::BucketIdentity;

        let prior = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS");
        // SAFETY: same justification as the S3 test above.
        unsafe {
            std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        }

        let mem = std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(mem.clone()).with_bucket_identity(BucketIdentity::new(
            Cloud::Gcs,
            "test-bucket",
            "test-bucket",
        ));

        let result = head_with_class(&store, &Path::from("any/key")).await;
        assert!(result.is_ok(), "must fall back to Ok rather than error");
        assert_eq!(result.unwrap().class, StorageClass::Unknown);

        // SAFETY: see comment above.
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", v);
            }
        }
    }

    /// Azure path: without `AZURE_STORAGE_ACCESS_KEY` the probe
    /// should degrade to `Unknown`, matching S3's behavior.
    #[cfg(feature = "tier-azure")]
    #[tokio::test]
    async fn azure_client_build_missing_key_falls_back_to_unknown() {
        use crate::git::url::Cloud;
        use crate::storage::store::BucketIdentity;

        let prior = std::env::var_os("AZURE_STORAGE_ACCESS_KEY");
        // SAFETY: same justification as the S3 test above.
        unsafe {
            std::env::remove_var("AZURE_STORAGE_ACCESS_KEY");
        }

        let mem = std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(mem.clone()).with_bucket_identity(BucketIdentity::new(
            Cloud::Azure,
            "testaccount",
            "test-container",
        ));

        let result = head_with_class(&store, &Path::from("any/blob")).await;
        assert!(result.is_ok(), "must fall back to Ok rather than error");
        assert_eq!(result.unwrap().class, StorageClass::Unknown);

        // SAFETY: see comment above.
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("AZURE_STORAGE_ACCESS_KEY", v);
            }
        }
    }
}
