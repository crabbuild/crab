//! Compatibility Adapter for Git and object URL parsing.
//!
//! The implementation lives in `crab-git`; this module preserves the existing
//! CLI-facing `CrabError` Interface while callers migrate to the Git-domain
//! `UrlError` Interface.

use object_store::path::Path as ObjectPath;

use crate::core::error::{CrabError, Result};
use crate::storage::store::BucketIdentity;

pub use crab_git::AzureStorageTarget;
pub use crab_git::url::{Cloud, UrlForm};

/// Parsed `crab://{bucket}/{repo-path}` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrabUrl {
    /// Cloud storage bucket name.
    pub bucket: String,
    /// Repository path within the bucket.
    pub repo_path: String,
}

impl CrabUrl {
    /// Parse a `crab://` URL string.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Configuration`] if the URL cannot be parsed,
    /// has the wrong scheme, is missing required components, or identifies a
    /// managed repository before the managed client path is enabled.
    pub fn parse(url: &str) -> Result<Self> {
        crab_git::RepositoryLocator::parse(url, |_| false)
            .and_then(crab_git::RepositoryLocator::require_direct)
            .map(|repository| Self {
                bucket: repository.bucket,
                repo_path: repository.repo_prefix,
            })
            .map_err(CrabError::from)
    }

    /// Extract bucket and repo path from an already-parsed `gix_url::Url`.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Configuration`] if the scheme is not
    /// `crab` or required components are missing.
    pub fn from_gix_url(url: &gix_url::Url) -> Result<Self> {
        crab_git::RepositoryLocator::from_gix_url(url, |_| false)
            .and_then(crab_git::RepositoryLocator::require_direct)
            .map(|repository| Self {
                bucket: repository.bucket,
                repo_path: repository.repo_prefix,
            })
            .map_err(CrabError::from)
    }

    /// Build an object-store path prefix from the repo path.
    #[must_use]
    pub fn object_prefix(&self) -> ObjectPath {
        ObjectPath::from(self.repo_path.as_str())
    }
}

impl From<crab_git::url::CrabUrl> for CrabUrl {
    fn from(url: crab_git::url::CrabUrl) -> Self {
        Self {
            bucket: url.bucket,
            repo_path: url.repo_path,
        }
    }
}

impl From<CrabUrl> for crab_git::url::CrabUrl {
    fn from(url: CrabUrl) -> Self {
        Self {
            bucket: url.bucket,
            repo_path: url.repo_path,
        }
    }
}

impl From<&CrabUrl> for crab_git::url::CrabUrl {
    fn from(url: &CrabUrl) -> Self {
        Self {
            bucket: url.bucket.clone(),
            repo_path: url.repo_path.clone(),
        }
    }
}

/// A scheme-polymorphic URL used by import, export, SDK, and CLI flows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectUrl {
    /// Raw cloud prefix vs Crab repo.
    pub form: UrlForm,
    /// Underlying physical cloud.
    pub cloud: Cloud,
    /// Bucket or container name; empty for `file://` URLs.
    pub bucket: String,
    /// Normalized object prefix or absolute local path.
    pub prefix: String,
}

impl ObjectUrl {
    /// Parse any of `s3://`, `gs://`, `az://`, `azure://`, `file://`,
    /// or `crab://`.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Configuration`] for malformed URLs,
    /// unsupported schemes, or missing required components.
    pub fn parse(url: &str) -> Result<Self> {
        crab_git::url::ObjectUrl::parse(url)
            .map(Self::from)
            .map_err(CrabError::from)
    }

    /// Stable identity for cross-scheme comparison.
    #[must_use]
    pub fn bucket_identity(&self) -> BucketIdentity {
        crab_git::url::ObjectUrl::from(self.clone()).bucket_identity()
    }

    /// Require a raw URL, erroring when the form is [`UrlForm::Crab`].
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::ImportSourceMustBeRaw`] for `crab://` URLs.
    pub fn require_raw(&self) -> Result<()> {
        crab_git::url::ObjectUrl::from(self.clone())
            .require_raw()
            .map_err(CrabError::from)
    }

    /// Interpret this raw Azure URL as account/container/prefix.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Configuration`] when the URL is not a raw Azure
    /// object URL or omits the container segment.
    pub fn azure_storage_target(&self) -> Result<AzureStorageTarget> {
        crab_git::url::ObjectUrl::from(self.clone())
            .azure_storage_target()
            .map_err(CrabError::from)
    }
}

impl From<crab_git::url::ObjectUrl> for ObjectUrl {
    fn from(url: crab_git::url::ObjectUrl) -> Self {
        Self {
            form: url.form,
            cloud: url.cloud,
            bucket: url.bucket,
            prefix: url.prefix,
        }
    }
}

impl From<ObjectUrl> for crab_git::url::ObjectUrl {
    fn from(url: ObjectUrl) -> Self {
        Self {
            form: url.form,
            cloud: url.cloud,
            bucket: url.bucket,
            prefix: url.prefix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_maps_domain_configuration_error_to_crab_error() {
        let err = CrabUrl::parse("https://bucket/repo").unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn require_raw_maps_domain_variant_to_crab_error() {
        let url = ObjectUrl::parse("crab://my-bucket/repo").unwrap();
        let err = url.require_raw().unwrap_err();
        assert!(matches!(err, CrabError::ImportSourceMustBeRaw { .. }));
    }

    #[test]
    fn hosted_managed_url_is_not_interpreted_as_direct_storage() {
        let error = CrabUrl::parse("crab://crab.build/acme/models").unwrap_err();

        assert!(matches!(error, CrabError::Configuration { .. }));
        assert!(
            error
                .to_string()
                .contains("managed repository support is not enabled")
        );
    }
}
