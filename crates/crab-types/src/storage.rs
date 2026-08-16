use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Physical storage provider behind a repository or object URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StorageProviderKind {
    /// AWS S3 or an S3-compatible endpoint.
    S3,
    /// Google Cloud Storage.
    Gcs,
    /// Azure Blob Storage.
    Azure,
    /// Local filesystem or in-memory storage used by tests.
    Local,
}

impl StorageProviderKind {
    /// Parse a user-facing cloud provider alias.
    ///
    /// Returns `None` for `local`/`file` because callers that build production
    /// object stores must opt into local storage explicitly.
    #[must_use]
    pub fn parse_cloud_alias(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "aws" | "s3" => Some(Self::S3),
            "gcp" | "gcs" | "gs" | "google" => Some(Self::Gcs),
            "azure" | "az" | "abs" => Some(Self::Azure),
            _ => None,
        }
    }
}

/// Stable identity for a cloud bucket, scheme-agnostic.
///
/// Two URLs that resolve to the same physical bucket produce equal identities
/// even when they use different URL schemes. `host` and `container` are
/// normalized at construction by lowercasing and trimming trailing slashes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BucketIdentity {
    /// Underlying physical storage provider.
    pub cloud: StorageProviderKind,
    /// Bucket or account host.
    pub host: String,
    /// Bucket or container name.
    pub container: String,
}

impl BucketIdentity {
    /// Builds an identity with normalized host and container fields.
    #[must_use]
    pub fn new(
        cloud: StorageProviderKind,
        host: impl Into<String>,
        container: impl Into<String>,
    ) -> Self {
        Self {
            cloud,
            host: normalize_identity_component(host.into()),
            container: normalize_identity_component(container.into()),
        }
    }

    /// Sentinel identity for local, in-memory, or otherwise unset stores.
    #[must_use]
    pub fn local_unset() -> Self {
        Self {
            cloud: StorageProviderKind::Local,
            host: String::new(),
            container: String::new(),
        }
    }
}

/// Scoped object-store prefixes issued by auth for path-limited repository views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageScope {
    pub repo_prefix: String,
    pub global_prefix: String,
    pub source_repo: String,
    pub scope_hash: String,
}

fn normalize_identity_component(mut value: String) -> String {
    while value.ends_with('/') {
        value.pop();
    }
    value.make_ascii_lowercase();
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_identity_normalizes_case_and_trailing_slashes() {
        let canonical = BucketIdentity::new(StorageProviderKind::S3, "my-bucket", "my-bucket");
        assert_eq!(
            canonical,
            BucketIdentity::new(StorageProviderKind::S3, "My-Bucket", "my-bucket")
        );
        assert_eq!(
            canonical,
            BucketIdentity::new(StorageProviderKind::S3, "my-bucket/", "MY-BUCKET/")
        );
    }

    #[test]
    fn bucket_identity_distinguishes_clouds() {
        let s3 = BucketIdentity::new(StorageProviderKind::S3, "foo", "foo");
        let gcs = BucketIdentity::new(StorageProviderKind::Gcs, "foo", "foo");
        let azure = BucketIdentity::new(StorageProviderKind::Azure, "foo", "foo");
        let local = BucketIdentity::new(StorageProviderKind::Local, "foo", "foo");

        assert_ne!(s3, gcs);
        assert_ne!(s3, azure);
        assert_ne!(s3, local);
    }

    #[test]
    fn local_unset_is_empty_local_identity() {
        assert_eq!(
            BucketIdentity::local_unset(),
            BucketIdentity {
                cloud: StorageProviderKind::Local,
                host: String::new(),
                container: String::new(),
            }
        );
    }

    #[test]
    fn storage_provider_kind_parses_cloud_aliases() {
        for alias in ["aws", "s3"] {
            assert_eq!(
                StorageProviderKind::parse_cloud_alias(alias),
                Some(StorageProviderKind::S3)
            );
        }
        for alias in ["gcp", "gcs", "gs", "google"] {
            assert_eq!(
                StorageProviderKind::parse_cloud_alias(alias),
                Some(StorageProviderKind::Gcs)
            );
        }
        for alias in ["azure", "az", "abs"] {
            assert_eq!(
                StorageProviderKind::parse_cloud_alias(alias),
                Some(StorageProviderKind::Azure)
            );
        }
    }

    #[test]
    fn storage_provider_kind_rejects_local_aliases_for_cloud_parse() {
        assert_eq!(StorageProviderKind::parse_cloud_alias("local"), None);
        assert_eq!(StorageProviderKind::parse_cloud_alias("file"), None);
    }
}
