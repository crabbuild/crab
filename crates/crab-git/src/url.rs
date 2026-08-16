//! Parsing for URLs that address objects in cloud storage.
//!
//! Repository and object URL parsers live here:
//!
//! - [`CrabUrl`] — the original `crab://{bucket}/{repo-path}`
//!   parser used by the remote helper and the push pipeline. Leaves
//!   `repo_path` non-empty because a Crab remote without a repo
//!   path is meaningless.
//! - [`RepositoryLocator`] — a typed managed-versus-direct classifier. Managed
//!   authorities use a strict logical repository grammar while unconfigured
//!   authorities delegate to [`CrabUrl`] unchanged.
//! - [`ObjectUrl`] — a scheme-polymorphic parser added for
//!   `crab import`. Accepts raw cloud schemes (`s3://`, `gs://`,
//!   `az://` / `azure://`, `file://`) and the `crab://` scheme,
//!   and normalizes the result so two URLs that resolve to the same
//!   physical bucket produce equal [`BucketIdentity`] values.
//!
//! `ObjectUrl` is additive: `CrabUrl::parse` stays in place for
//! callers that want the stricter contract. When `ObjectUrl::parse`
//! sees a `crab://` URL it delegates the bucket / prefix split to
//! `CrabUrl::parse` so the two parsers can't disagree.
//!
//! The [`Cloud`] name is a compatibility alias for the shared storage
//! provider-kind contract in `crab-types`; URL parsing decides the value,
//! but storage identity and provider construction also need it.

use crab_types::storage::BucketIdentity;
use gix_url::Scheme;

pub use crab_types::storage::StorageProviderKind as Cloud;

/// Result alias for Crab Git URL parsing.
pub type Result<T> = std::result::Result<T, UrlError>;

/// Errors raised while parsing Crab Git and object-store URLs.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    /// The input could not be parsed by gitoxide.
    #[error("invalid crab URL: {message}")]
    InvalidCrabUrl { origin: String, message: String },

    /// A parsed URL did not use the `crab://` scheme.
    #[error("expected crab:// scheme, got {actual}")]
    ExpectedCrabScheme { origin: String, actual: String },

    /// A URL omitted its bucket or host component.
    #[error("missing bucket (host) in crab URL")]
    MissingBucket { origin: String },

    /// A `crab://` URL omitted its repository path.
    #[error("missing repo path in crab URL")]
    MissingRepoPath { origin: String },

    /// A reserved or configured managed URL violated the logical URL grammar.
    #[error("invalid managed repository URL for {authority}: {message}")]
    InvalidManagedRepository { authority: String, message: String },

    /// A caller reached a managed locator before managed resolution was wired.
    #[error(
        "managed repository support is not enabled for crab://{authority}/{organization}/{repository}"
    )]
    ManagedServiceNotEnabled {
        authority: String,
        organization: String,
        repository: String,
    },

    /// The caller supplied an empty URL string.
    #[error("empty URL")]
    EmptyUrl { origin: String },

    /// The caller supplied text without a scheme separator.
    #[error("missing scheme separator in URL: {input}")]
    MissingSchemeSeparator { origin: String, input: String },

    /// The URL scheme is not one Crab understands.
    #[error("unsupported URL scheme {scheme:?}: expected s3, gs, az, azure, file, or crab")]
    UnsupportedScheme { origin: String, scheme: String },

    /// A repository URL scheme is not valid for a Crab repository.
    #[error(
        "unsupported repository URL scheme {scheme:?}: expected crab, s3, gs, gcs, az, or azure"
    )]
    UnsupportedRepositoryScheme { origin: String, scheme: String },

    /// A cloud URL omitted its bucket or container.
    #[error("missing bucket in URL")]
    MissingObjectBucket { origin: String },

    /// A repository URL bucket/container name is malformed.
    #[error("invalid repository bucket: {message}")]
    InvalidRepositoryBucket { origin: String, message: String },

    /// A repository URL path prefix is malformed.
    #[error("invalid repository prefix: {message}")]
    InvalidRepositoryPrefix { origin: String, message: String },

    /// A `file://` URL omitted its absolute filesystem path.
    #[error("file:// URL missing absolute path")]
    FileMissingAbsolutePath { origin: String },

    /// A raw object URL was required, but the URL named a Crab repo.
    #[error("import source must be a raw object-store URL, got {url}")]
    ImportSourceMustBeRaw { url: String },

    /// An Azure object URL was required, but the URL named another provider.
    #[error("expected Azure object URL, got {url}")]
    ExpectedAzureObjectUrl { url: String },

    /// A raw Azure object URL omitted the container path segment.
    #[error("Azure object URL {url} must use az://account/container[/repo-prefix]")]
    MissingAzureContainer { url: String },
}

/// Parsed `crab://{bucket}/{repo-path}` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrabUrl {
    /// Cloud storage bucket name (the host portion of the URL).
    pub bucket: String,
    /// Repository path within the bucket.
    pub repo_path: String,
}

/// Logical identity of a repository resolved through a managed Crab service.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagedRepository {
    /// Canonical service authority, normalized to lowercase.
    pub authority: String,
    /// Normalized organization slug.
    pub organization: String,
    /// Normalized repository slug.
    pub repository: String,
}

/// Physical object-store identity of a directly configured Crab repository.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DirectRepository {
    /// Cloud storage bucket name.
    pub bucket: String,
    /// Repository prefix inside the bucket.
    pub repo_prefix: String,
}

/// A Crab repository classified before any object store is constructed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RepositoryLocator {
    /// Repository whose physical placement must be resolved by a service.
    Managed(ManagedRepository),
    /// Repository whose URL directly names its object-store location.
    Direct(DirectRepository),
}

/// Parsed URL that names a Crab repository in cloud object storage.
///
/// This accepts `crab://` plus raw cloud schemes used by server-side helpers.
/// It validates only the bucket and repo-prefix shape; provider selection stays
/// at the caller's composition seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryUrl {
    /// Cloud storage bucket or container name.
    pub bucket: String,
    /// Repository path within the bucket/container.
    pub repo_prefix: String,
}

impl RepositoryUrl {
    /// Parse a repository URL into bucket and repo prefix.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError`] for unsupported schemes, missing bucket/prefix, or
    /// unsafe path components.
    pub fn parse(url: &str) -> Result<Self> {
        let trimmed = url.trim();
        let Some((scheme_raw, rest)) = trimmed.split_once("://") else {
            return Err(UrlError::MissingSchemeSeparator {
                origin: url.to_owned(),
                input: trimmed.to_owned(),
            });
        };
        validate_repository_scheme(url, scheme_raw)?;
        let (bucket, repo_prefix) =
            rest.split_once('/')
                .ok_or_else(|| UrlError::MissingRepoPath {
                    origin: url.to_owned(),
                })?;
        Ok(Self {
            bucket: normalize_repository_bucket(bucket)?,
            repo_prefix: normalize_repository_prefix(repo_prefix)?,
        })
    }
}

impl CrabUrl {
    /// Parse a `crab://` URL string.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError`] if the URL cannot be parsed,
    /// has the wrong scheme, or is missing required components.
    pub fn parse(url: &str) -> Result<Self> {
        let parsed = gix_url::Url::from_bytes(url.as_bytes().into()).map_err(|e| {
            UrlError::InvalidCrabUrl {
                origin: url.to_owned(),
                message: e.to_string(),
            }
        })?;

        Self::from_gix_url(&parsed)
    }

    /// Extract bucket and repo-path from an already-parsed `gix_url::Url`.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError`] if the scheme is not
    /// `crab` or required components are missing.
    pub fn from_gix_url(url: &gix_url::Url) -> Result<Self> {
        // Verify scheme is Ext("crab").
        match &url.scheme {
            Scheme::Ext(name) if name == "crab" => {}
            other => {
                return Err(UrlError::ExpectedCrabScheme {
                    origin: url.to_bstring().to_string(),
                    actual: other.to_string(),
                });
            }
        }

        let bucket = url
            .host()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| UrlError::MissingBucket {
                origin: url.to_bstring().to_string(),
            })?
            .to_owned();

        // The path from gix-url includes a leading `/`. Strip it and
        // also strip any trailing `/` for a clean repo path.
        let raw_path = url.path.to_string();
        let repo_path = raw_path
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_owned();

        if repo_path.is_empty() {
            return Err(UrlError::MissingRepoPath {
                origin: url.to_bstring().to_string(),
            });
        }

        Ok(Self { bucket, repo_path })
    }

    /// Return the object prefix under which repository objects are stored.
    #[must_use]
    pub fn object_prefix(&self) -> &str {
        self.repo_path.as_str()
    }
}

impl RepositoryLocator {
    /// Parse and classify a Crab repository URL.
    ///
    /// `has_service_profile` must perform an in-memory lookup of installed
    /// service profiles by canonical authority. It must not discover or probe a
    /// network endpoint. `crab.build` is always managed and does not consult the
    /// lookup.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError`] when the URL is malformed. Reserved or configured
    /// managed authorities never fall back to the direct interpretation.
    pub fn parse(url: &str, has_service_profile: impl FnOnce(&str) -> bool) -> Result<Self> {
        let parsed = gix_url::Url::from_bytes(url.as_bytes().into()).map_err(|error| {
            UrlError::InvalidCrabUrl {
                origin: url.to_owned(),
                message: error.to_string(),
            }
        })?;
        Self::from_gix_url(&parsed, has_service_profile)
    }

    /// Classify an already parsed Crab URL using installed service profiles.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError`] for a non-Crab URL, a missing authority, or an
    /// invalid managed locator.
    pub fn from_gix_url(
        url: &gix_url::Url,
        has_service_profile: impl FnOnce(&str) -> bool,
    ) -> Result<Self> {
        match &url.scheme {
            Scheme::Ext(name) if name == "crab" => {}
            other => {
                return Err(UrlError::ExpectedCrabScheme {
                    origin: url.to_bstring().to_string(),
                    actual: other.to_string(),
                });
            }
        }

        let authority = url
            .host()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| UrlError::MissingBucket {
                origin: url.to_bstring().to_string(),
            })?
            .to_ascii_lowercase();
        let is_managed =
            authority.eq_ignore_ascii_case("crab.build") || has_service_profile(authority.as_str());
        if !is_managed {
            return CrabUrl::from_gix_url(url).map(|direct| {
                Self::Direct(DirectRepository {
                    bucket: direct.bucket,
                    repo_prefix: direct.repo_path,
                })
            });
        }

        parse_managed_repository(url, authority).map(Self::Managed)
    }

    /// Require the direct-storage variant.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError::ManagedServiceNotEnabled`] for a managed repository.
    pub fn require_direct(self) -> Result<DirectRepository> {
        match self {
            Self::Direct(repository) => Ok(repository),
            Self::Managed(repository) => Err(UrlError::ManagedServiceNotEnabled {
                authority: repository.authority,
                organization: repository.organization,
                repository: repository.repository,
            }),
        }
    }

    /// Returns the stable repository URL represented by this locator.
    #[must_use]
    pub fn canonical_url(&self) -> String {
        match self {
            Self::Managed(repository) => repository.canonical_url(),
            Self::Direct(repository) => {
                format!("crab://{}/{}", repository.bucket, repository.repo_prefix)
            }
        }
    }
}

impl ManagedRepository {
    /// Builds a managed repository from one validated authority and two slugs.
    pub fn new(authority: &str, organization: &str, repository: &str) -> Result<Self> {
        let authority = authority.to_ascii_lowercase();
        if !is_normalized_managed_authority(&authority) {
            return Err(invalid_managed(authority, "authority is invalid"));
        }
        if !is_normalized_managed_slug(organization) || !is_normalized_managed_slug(repository) {
            return Err(invalid_managed(
                authority,
                "organization and repository must be lowercase slugs",
            ));
        }
        Ok(Self {
            authority,
            organization: organization.to_owned(),
            repository: repository.to_owned(),
        })
    }

    /// Return the stable canonical logical URL.
    #[must_use]
    pub fn canonical_url(&self) -> String {
        format!(
            "crab://{}/{}/{}",
            self.authority, self.organization, self.repository
        )
    }
}

impl DirectRepository {
    /// Return the object prefix under which repository objects are stored.
    #[must_use]
    pub fn object_prefix(&self) -> &str {
        self.repo_prefix.as_str()
    }
}

impl From<CrabUrl> for DirectRepository {
    fn from(url: CrabUrl) -> Self {
        Self {
            bucket: url.bucket,
            repo_prefix: url.repo_path,
        }
    }
}

impl From<DirectRepository> for CrabUrl {
    fn from(repository: DirectRepository) -> Self {
        Self {
            bucket: repository.bucket,
            repo_path: repository.repo_prefix,
        }
    }
}

fn parse_managed_repository(url: &gix_url::Url, authority: String) -> Result<ManagedRepository> {
    if url.user.is_some() || url.password.is_some() {
        return Err(invalid_managed(
            authority,
            "user information is not allowed",
        ));
    }
    if url.port.is_some() {
        return Err(invalid_managed(authority, "ports are not allowed"));
    }

    let path = url.path.to_string();
    let Some(path) = path.strip_prefix('/') else {
        return Err(invalid_managed(authority, "path must begin with one slash"));
    };
    if path.starts_with('/') || path.ends_with('/') {
        return Err(invalid_managed(
            authority,
            "path must contain exactly two non-empty segments",
        ));
    }
    let mut segments = path.split('/');
    let organization = segments.next().unwrap_or_default();
    let repository = segments.next().unwrap_or_default();
    if organization.is_empty() || repository.is_empty() || segments.next().is_some() {
        return Err(invalid_managed(
            authority,
            "path must contain exactly two non-empty segments",
        ));
    }
    if !is_normalized_managed_slug(organization) || !is_normalized_managed_slug(repository) {
        return Err(invalid_managed(
            authority,
            "organization and repository must be lowercase slugs",
        ));
    }

    Ok(ManagedRepository {
        authority,
        organization: organization.to_owned(),
        repository: repository.to_owned(),
    })
}

fn invalid_managed(authority: String, message: &str) -> UrlError {
    UrlError::InvalidManagedRepository {
        authority,
        message: message.to_owned(),
    }
}

fn is_normalized_managed_slug(slug: &str) -> bool {
    !slug.is_empty()
        && !matches!(slug, "." | "..")
        && slug.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '@' | '+' | '=' | ',' | '-')
        })
}

fn is_normalized_managed_authority(authority: &str) -> bool {
    !authority.is_empty()
        && authority.len() <= 253
        && authority.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

fn validate_repository_scheme(origin: &str, scheme: &str) -> Result<()> {
    match scheme.to_ascii_lowercase().as_str() {
        "crab" | "s3" | "gs" | "gcs" | "az" | "azure" => Ok(()),
        other => Err(UrlError::UnsupportedRepositoryScheme {
            origin: origin.to_owned(),
            scheme: other.to_owned(),
        }),
    }
}

/// Normalize and validate a repository bucket or container name.
///
/// # Errors
///
/// Returns [`UrlError::InvalidRepositoryBucket`] when the bucket is empty,
/// path-shaped, or contains unsupported characters.
pub fn normalize_repository_bucket(bucket: &str) -> Result<String> {
    let normalized = bucket.trim();
    if normalized.is_empty() || normalized == "." || normalized == ".." || normalized.contains('/')
    {
        return Err(UrlError::InvalidRepositoryBucket {
            origin: bucket.to_owned(),
            message: "repository URL must include a bucket or container".into(),
        });
    }
    let mut chars = normalized.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(UrlError::InvalidRepositoryBucket {
            origin: bucket.to_owned(),
            message: "bucket or container name contains unsupported characters".into(),
        });
    }
    Ok(normalized.to_owned())
}

/// Normalize and validate a repository object prefix.
///
/// # Errors
///
/// Returns [`UrlError::InvalidRepositoryPrefix`] when the prefix is empty, too
/// long, contains unsafe path components, or uses unsupported characters.
pub fn normalize_repository_prefix(prefix: &str) -> Result<String> {
    let normalized = prefix.trim().trim_matches('/');
    if normalized.is_empty() {
        return Err(UrlError::InvalidRepositoryPrefix {
            origin: prefix.to_owned(),
            message: "repository URL must include bucket and repo prefix".into(),
        });
    }
    if normalized.len() > 1024 {
        return Err(UrlError::InvalidRepositoryPrefix {
            origin: prefix.to_owned(),
            message: "repo prefix is too long".into(),
        });
    }
    for segment in normalized.split('/') {
        if segment.is_empty() || matches!(segment, "." | "..") {
            return Err(UrlError::InvalidRepositoryPrefix {
                origin: prefix.to_owned(),
                message: "repo prefix contains an unsafe path component".into(),
            });
        }
        if !segment.chars().all(is_safe_repo_segment_char) {
            return Err(UrlError::InvalidRepositoryPrefix {
                origin: prefix.to_owned(),
                message: "repo prefix contains unsupported characters; use letters, numbers, '/', '.', '_', '-', '@', '+', '=', or ','".into(),
            });
        }
    }
    Ok(normalized.to_owned())
}

fn is_safe_repo_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '+' | '=' | ',' | '-')
}

// --- ObjectUrl ---

/// Whether an [`ObjectUrl`] names raw cloud objects or a Crab repo.
///
/// `crab import --from` requires `Raw`; `crab import --to`
/// accepts either form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UrlForm {
    /// A raw cloud prefix — the URL points at objects the user owns
    /// directly (`s3://bucket/key/prefix`, `file:///path`, …).
    Raw,
    /// A Crab repo URL (`crab://bucket/prefix`).
    Crab,
}

/// A scheme-polymorphic URL used by the import pipeline.
///
/// Parsed form is normalized: bucket / container names are
/// lowercased with trailing slashes stripped, prefixes carry no
/// leading or trailing `/`. Two URLs that resolve to the same
/// physical bucket produce equal [`BucketIdentity`] values via
/// [`ObjectUrl::bucket_identity`].
///
/// For `file://` URLs, `bucket` is empty and `prefix` carries the
/// absolute path. The identity uses the prefix itself as the
/// container so two `file://` sources at the same path are treated
/// as the same "bucket" for same-source detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectUrl {
    /// Raw cloud prefix vs Crab repo.
    pub form: UrlForm,
    /// Underlying physical cloud — S3, GCS, Azure, or Local.
    pub cloud: Cloud,
    /// Bucket or container name; empty for `file://` URLs.
    pub bucket: String,
    /// Object prefix, possibly empty. No leading or trailing `/`.
    pub prefix: String,
}

/// Account, container, and object prefix extracted from a raw Azure URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureStorageTarget {
    /// Azure storage account name.
    pub account: String,
    /// Azure blob container name.
    pub container: String,
    /// Optional object prefix inside the container.
    pub object_prefix: String,
}

impl ObjectUrl {
    /// Parse any of `s3://`, `gs://`, `az://`, `azure://`, `file://`,
    /// or `crab://`.
    ///
    /// For `crab://`, the bucket / prefix split delegates to
    /// [`CrabUrl::parse`]. The cloud backing a `crab://` URL
    /// cannot be derived from the URL alone — provider resolution
    /// for `crab://` is an open question in the `crab-bucket-
    /// import` spec. V1 defaults to [`Cloud::S3`] so downstream code
    /// has a concrete value to work with; once the config-driven
    /// resolver lands, that plumbing replaces this default.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError`] for malformed URLs,
    /// unsupported schemes (`https://…`), or missing required
    /// components (bucket for cloud schemes, absolute path for
    /// `file://`).
    pub fn parse(url: &str) -> Result<Self> {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return Err(UrlError::EmptyUrl {
                origin: url.to_owned(),
            });
        }

        // Split on the first `://`. Anything without a scheme
        // separator is not a URL we want to accept silently.
        let Some((scheme_raw, rest)) = trimmed.split_once("://") else {
            return Err(UrlError::MissingSchemeSeparator {
                origin: url.to_owned(),
                input: trimmed.to_owned(),
            });
        };

        let scheme = scheme_raw.to_ascii_lowercase();
        match scheme.as_str() {
            "s3" => parse_cloud_url(url, rest, Cloud::S3),
            "gs" => parse_cloud_url(url, rest, Cloud::Gcs),
            "az" | "azure" => parse_cloud_url(url, rest, Cloud::Azure),
            "file" => parse_file_url(url, rest),
            "crab" => parse_crab_url(url),
            other => Err(UrlError::UnsupportedScheme {
                origin: url.to_owned(),
                scheme: other.to_owned(),
            }),
        }
    }

    /// Stable identity for cross-scheme comparison.
    ///
    /// `s3://my-bucket/a` and `crab://my-bucket/b` produce equal
    /// identities when their underlying cloud and bucket match;
    /// the repo prefix is intentionally excluded so same-bucket
    /// detection fires regardless of which sub-prefix the user
    /// picks.
    #[must_use]
    pub fn bucket_identity(&self) -> BucketIdentity {
        match self.cloud {
            Cloud::Local => {
                // `file://` has no bucket in the usual sense. Use the
                // prefix (the absolute path) as both host and
                // container so two imports from the same local root
                // compare equal, and two distinct local roots don't.
                BucketIdentity::new(self.cloud, self.prefix.as_str(), self.prefix.as_str())
            }
            _ => BucketIdentity::new(self.cloud, self.bucket.as_str(), self.bucket.as_str()),
        }
    }

    /// Require a raw URL, erroring when the form is [`UrlForm::Crab`].
    ///
    /// `crab import --from` uses this to reject `crab://` URLs
    /// up front — the correct command for a Crab repo source is
    /// `crab clone`, not `crab import`.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError::ImportSourceMustBeRaw`] when called on
    /// a `crab://` URL.
    pub fn require_raw(&self) -> Result<()> {
        match self.form {
            UrlForm::Raw => Ok(()),
            UrlForm::Crab => Err(UrlError::ImportSourceMustBeRaw {
                url: self.render_for_error(),
            }),
        }
    }

    /// Interpret this raw Azure URL as account/container/prefix.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError::ExpectedAzureObjectUrl`] when the URL is not a raw
    /// Azure object URL, and [`UrlError::MissingAzureContainer`] when the raw
    /// URL omits the container segment.
    pub fn azure_storage_target(&self) -> Result<AzureStorageTarget> {
        if self.cloud != Cloud::Azure || self.form != UrlForm::Raw {
            return Err(UrlError::ExpectedAzureObjectUrl {
                url: self.render_for_error(),
            });
        }
        let (container, object_prefix) = self
            .prefix
            .split_once('/')
            .map_or((self.prefix.as_str(), ""), |(container, object_prefix)| {
                (container, object_prefix)
            });
        if container.is_empty() {
            return Err(UrlError::MissingAzureContainer {
                url: self.render_for_error(),
            });
        }
        Ok(AzureStorageTarget {
            account: self.bucket.clone(),
            container: container.to_ascii_lowercase(),
            object_prefix: object_prefix.trim_matches('/').to_owned(),
        })
    }

    /// Return the repository prefix implied by this object URL.
    ///
    /// Raw Azure URLs use `az://account/container[/repo-prefix]`, so the first
    /// path segment names the container rather than the repository prefix. For
    /// all other object URLs, the normalized URL prefix is already the
    /// repository prefix. When the URL does not carry a repository prefix, the
    /// supplied default is returned unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`UrlError::MissingAzureContainer`] when a raw Azure URL omits
    /// the required container segment.
    pub fn effective_repo_prefix(&self, default_repo_prefix: &str) -> Result<String> {
        if self.cloud == Cloud::Azure && self.form == UrlForm::Raw {
            let target = self.azure_storage_target()?;
            return Ok(if target.object_prefix.is_empty() {
                default_repo_prefix.to_owned()
            } else {
                target.object_prefix
            });
        }
        Ok(if self.prefix.is_empty() {
            default_repo_prefix.to_owned()
        } else {
            self.prefix.clone()
        })
    }

    /// Render a best-effort URL string for error messages. Uses the
    /// normalized components rather than the original input so the
    /// error message reflects what the parser actually saw.
    fn render_for_error(&self) -> String {
        let scheme = match (self.form, self.cloud) {
            (UrlForm::Crab, _) => "crab",
            (UrlForm::Raw, Cloud::S3) => "s3",
            (UrlForm::Raw, Cloud::Gcs) => "gs",
            (UrlForm::Raw, Cloud::Azure) => "az",
            (UrlForm::Raw, Cloud::Local) => "file",
        };
        if self.cloud == Cloud::Local {
            // Local paths carry their absolute path in `prefix`; the
            // canonical form is `file:///<path>`.
            format!("file:///{}", self.prefix.trim_start_matches('/'))
        } else if self.prefix.is_empty() {
            format!("{scheme}://{}", self.bucket)
        } else {
            format!("{scheme}://{}/{}", self.bucket, self.prefix)
        }
    }
}

/// Parse `rest` of a cloud URL where `rest = bucket[/prefix…]`.
fn parse_cloud_url(input: &str, rest: &str, cloud: Cloud) -> Result<ObjectUrl> {
    // Split bucket from prefix. `rest` never contains the scheme.
    let (bucket_raw, prefix_raw) = match rest.split_once('/') {
        Some((b, p)) => (b, p),
        None => (rest, ""),
    };

    if bucket_raw.is_empty() {
        return Err(UrlError::MissingObjectBucket {
            origin: input.to_owned(),
        });
    }

    let bucket = normalize_bucket(bucket_raw);
    let prefix = normalize_prefix(prefix_raw);

    Ok(ObjectUrl {
        form: UrlForm::Raw,
        cloud,
        bucket,
        prefix,
    })
}

/// Parse the `rest` of a `file://` URL per RFC 8089.
///
/// Accepts `file:///absolute/path` (three slashes) canonically and
/// tolerates `file://host/absolute/path` by ignoring the host. Bucket is
/// always empty; prefix carries the absolute path with a single leading `/`.
fn parse_file_url(input: &str, rest: &str) -> Result<ObjectUrl> {
    // `rest` has the scheme stripped. For `file:///abs/path` it
    // starts with `/abs/path`; for `file://host/abs/path` it starts
    // with `host/abs/path`.
    let path = if let Some(stripped) = rest.strip_prefix('/') {
        // Canonical `file:///abs/path` — `rest` was `/abs/path`;
        // stripped is `abs/path`. Re-add the leading slash below.
        stripped.to_owned()
    } else if let Some((_host, after_host)) = rest.split_once('/') {
        // `file://host/abs/path`.
        after_host.to_owned()
    } else {
        // `file://` on its own or `file://host` — no path.
        return Err(UrlError::FileMissingAbsolutePath {
            origin: input.to_owned(),
        });
    };

    // Strip trailing slashes; keep a single leading `/` so the
    // stored prefix is an absolute path.
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(UrlError::FileMissingAbsolutePath {
            origin: input.to_owned(),
        });
    }

    let prefix = format!("/{trimmed}");

    Ok(ObjectUrl {
        form: UrlForm::Raw,
        cloud: Cloud::Local,
        bucket: String::new(),
        prefix,
    })
}

/// Parse a `crab://` URL by delegating to [`CrabUrl::parse`].
///
/// Provider resolution for `crab://` is an open question in the
/// `crab-bucket-import` spec: the URL alone doesn't tell us which
/// cloud backs the repo. V1 defaults to [`Cloud::S3`]; a later
/// change reads `Config.storage.provider` or a bucket registry and
/// replaces this stub.
fn parse_crab_url(input: &str) -> Result<ObjectUrl> {
    let direct = RepositoryLocator::parse(input, |_| false)?.require_direct()?;
    Ok(ObjectUrl {
        form: UrlForm::Crab,
        cloud: Cloud::S3,
        bucket: normalize_bucket(&direct.bucket),
        prefix: normalize_prefix(&direct.repo_prefix),
    })
}

fn normalize_bucket(raw: &str) -> String {
    let mut out = raw.trim_end_matches('/').to_ascii_lowercase();
    // Defensive: also drop any stray leading slash.
    while out.starts_with('/') {
        out.remove(0);
    }
    out
}

fn normalize_prefix(raw: &str) -> String {
    let trimmed = raw.trim_matches('/');
    trimmed.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_repository_constructor_requires_exact_normalized_parts() {
        let repository = ManagedRepository::new("code.corp.example", "ml-team", "models").unwrap();
        assert_eq!(
            repository.canonical_url(),
            "crab://code.corp.example/ml-team/models"
        );
        assert!(ManagedRepository::new("code.corp.example", "ML-Team", "models").is_err());
        assert!(ManagedRepository::new("code/corp", "ml-team", "models").is_err());
    }

    // --- CrabUrl (existing tests, unchanged) ---

    #[test]
    fn parse_simple_url() {
        let url = CrabUrl::parse("crab://my-bucket/my-repo").unwrap();
        assert_eq!(url.bucket, "my-bucket");
        assert_eq!(url.repo_path, "my-repo");
    }

    #[test]
    fn parse_nested_repo_path() {
        let url = CrabUrl::parse("crab://my-bucket/org/project/repo").unwrap();
        assert_eq!(url.bucket, "my-bucket");
        assert_eq!(url.repo_path, "org/project/repo");
    }

    #[test]
    fn parse_strips_trailing_slash() {
        let url = CrabUrl::parse("crab://my-bucket/repo/").unwrap();
        assert_eq!(url.repo_path, "repo");
    }

    #[test]
    fn object_prefix_matches_repo_path() {
        let url = CrabUrl::parse("crab://bucket/some/path").unwrap();
        assert_eq!(url.object_prefix(), "some/path");
    }

    #[test]
    fn wrong_scheme_rejected() {
        let err = CrabUrl::parse("https://bucket/repo").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("expected crab:// scheme"), "got: {msg}");
    }

    #[test]
    fn missing_bucket_rejected() {
        let err = CrabUrl::parse("crab:///repo").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing bucket"), "got: {msg}");
    }

    #[test]
    fn missing_repo_path_rejected() {
        let err = CrabUrl::parse("crab://bucket/").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing repo path"), "got: {msg}");
    }

    #[test]
    fn from_gix_url_works() {
        let gix = gix_url::Url::from_bytes(b"crab://bucket/repo".into()).unwrap();
        let url = CrabUrl::from_gix_url(&gix).unwrap();
        assert_eq!(url.bucket, "bucket");
        assert_eq!(url.repo_path, "repo");
    }

    #[test]
    fn bucket_only_no_path_rejected() {
        let err = CrabUrl::parse("crab://bucket").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing repo path") || msg.contains("missing bucket"),
            "got: {msg}"
        );
    }

    #[test]
    fn repository_locator_classifies_hosted_and_direct_urls() {
        let cases = [
            (
                "crab://crab.build/acme/models",
                RepositoryLocator::Managed(ManagedRepository {
                    authority: "crab.build".to_owned(),
                    organization: "acme".to_owned(),
                    repository: "models".to_owned(),
                }),
            ),
            (
                "crab://CRAB.BUILD/acme/models",
                RepositoryLocator::Managed(ManagedRepository {
                    authority: "crab.build".to_owned(),
                    organization: "acme".to_owned(),
                    repository: "models".to_owned(),
                }),
            ),
            (
                "crab://team-bucket/ml/models",
                RepositoryLocator::Direct(DirectRepository {
                    bucket: "team-bucket".to_owned(),
                    repo_prefix: "ml/models".to_owned(),
                }),
            ),
            (
                "crab://team-bucket/org/project/repo",
                RepositoryLocator::Direct(DirectRepository {
                    bucket: "team-bucket".to_owned(),
                    repo_prefix: "org/project/repo".to_owned(),
                }),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(
                RepositoryLocator::parse(input, |_| false).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn repository_locator_rejects_every_ambiguous_managed_shape() {
        for input in [
            "crab://crab.build/acme",
            "crab://crab.build/acme/models/extra",
            "crab://crab.build//acme/models",
            "crab://crab.build/acme/models/",
            "crab://user@crab.build/acme/models",
            "crab://user:secret@crab.build/acme/models",
            "crab://crab.build:443/acme/models",
            "crab://crab.build/acme/models?view=latest",
            "crab://crab.build/acme/models#main",
            "crab://crab.build/Acme/models",
            "crab://crab.build/acme/Models",
            "crab://crab.build/acme%2Fmodels/repo",
        ] {
            assert!(
                matches!(
                    RepositoryLocator::parse(input, |_| false),
                    Err(UrlError::InvalidManagedRepository { .. })
                ),
                "expected {input} to be rejected as managed"
            );
        }
    }

    #[test]
    fn repository_locator_uses_only_explicit_custom_authority_profiles() {
        let input = "crab://code.corp.example/ml/models";
        let managed =
            RepositoryLocator::parse(input, |authority| authority == "code.corp.example").unwrap();
        let direct = RepositoryLocator::parse(input, |_| false).unwrap();

        assert_eq!(
            managed,
            RepositoryLocator::Managed(ManagedRepository {
                authority: "code.corp.example".to_owned(),
                organization: "ml".to_owned(),
                repository: "models".to_owned(),
            })
        );
        assert_eq!(
            direct,
            RepositoryLocator::Direct(DirectRepository {
                bucket: "code.corp.example".to_owned(),
                repo_prefix: "ml/models".to_owned(),
            })
        );
    }

    #[test]
    fn configured_custom_authority_never_falls_back_after_validation_failure() {
        let result =
            RepositoryLocator::parse("crab://code.corp.example/ml/models/extra", |authority| {
                authority == "code.corp.example"
            });

        assert!(matches!(
            result,
            Err(UrlError::InvalidManagedRepository { .. })
        ));
    }

    #[test]
    fn managed_repository_renders_canonical_url() {
        let repository = ManagedRepository {
            authority: "crab.build".to_owned(),
            organization: "acme".to_owned(),
            repository: "models".to_owned(),
        };

        assert_eq!(repository.canonical_url(), "crab://crab.build/acme/models");
    }

    #[test]
    fn repository_locator_renders_canonical_managed_and_direct_urls() {
        let cases = [
            (
                RepositoryLocator::Managed(
                    ManagedRepository::new("crab.build", "acme", "models").unwrap(),
                ),
                "crab://crab.build/acme/models",
            ),
            (
                RepositoryLocator::Direct(DirectRepository {
                    bucket: "team-bucket".to_owned(),
                    repo_prefix: "ml/models".to_owned(),
                }),
                "crab://team-bucket/ml/models",
            ),
        ];

        for (locator, expected) in cases {
            assert_eq!(locator.canonical_url(), expected);
        }
    }

    #[test]
    fn managed_repository_cannot_be_required_as_direct() {
        let error = RepositoryLocator::parse("crab://crab.build/acme/models", |_| false)
            .unwrap()
            .require_direct()
            .unwrap_err();

        assert_eq!(
            error,
            UrlError::ManagedServiceNotEnabled {
                authority: "crab.build".to_owned(),
                organization: "acme".to_owned(),
                repository: "models".to_owned(),
            }
        );
    }

    #[test]
    fn object_url_never_reinterprets_hosted_managed_url_as_s3() {
        assert!(matches!(
            ObjectUrl::parse("crab://crab.build/acme/models"),
            Err(UrlError::ManagedServiceNotEnabled { .. })
        ));
    }

    #[test]
    fn repository_url_accepts_supported_repo_schemes() {
        for (input, bucket) in [
            ("crab://bucket/org/repo", "bucket"),
            ("s3://bucket/org/repo", "bucket"),
            ("gs://bucket/org/repo", "bucket"),
            ("gcs://bucket/org/repo", "bucket"),
            ("az://container/org/repo", "container"),
            ("azure://container/org/repo", "container"),
        ] {
            let url = RepositoryUrl::parse(input).unwrap();
            assert_eq!(url.bucket, bucket);
            assert_eq!(url.repo_prefix, "org/repo");
        }
    }

    #[test]
    fn repository_url_rejects_non_repo_schemes() {
        let err = RepositoryUrl::parse("file:///tmp/repo").unwrap_err();
        assert!(matches!(err, UrlError::UnsupportedRepositoryScheme { .. }));
    }

    #[test]
    fn repository_url_rejects_unsafe_bucket_and_prefix_shapes() {
        for input in [
            "bucket/org/repo",
            "https://bucket/org/repo",
            "crab://bucket",
            "crab://bucket/org/*",
            "crab://bucket/org/../repo",
            "crab://bucket/org//repo",
            "crab://./org/repo",
        ] {
            assert!(
                RepositoryUrl::parse(input).is_err(),
                "expected {input} to be rejected"
            );
        }
    }

    #[test]
    fn repository_prefix_normalizes_scope_values() {
        assert_eq!(
            normalize_repository_prefix(" /org/repo/ ").unwrap(),
            "org/repo"
        );
        assert!(normalize_repository_prefix("org/../repo").is_err());
        assert!(normalize_repository_prefix("org//repo").is_err());
    }

    // --- ObjectUrl ---

    #[test]
    fn object_url_parses_s3_nested_prefix() {
        let url = ObjectUrl::parse("s3://my-bucket/a/b/c").unwrap();
        assert_eq!(url.form, UrlForm::Raw);
        assert_eq!(url.cloud, Cloud::S3);
        assert_eq!(url.bucket, "my-bucket");
        assert_eq!(url.prefix, "a/b/c");
    }

    #[test]
    fn object_url_parses_gs() {
        let url = ObjectUrl::parse("gs://data/raw").unwrap();
        assert_eq!(url.cloud, Cloud::Gcs);
        assert_eq!(url.bucket, "data");
        assert_eq!(url.prefix, "raw");
    }

    #[test]
    fn object_url_parses_az_and_azure_alias() {
        let az = ObjectUrl::parse("az://container/path").unwrap();
        let azure = ObjectUrl::parse("azure://container/path").unwrap();
        assert_eq!(az.cloud, Cloud::Azure);
        assert_eq!(azure.cloud, Cloud::Azure);
        // Both normalize to the same ObjectUrl (scheme is not part of
        // the struct; the cloud is).
        assert_eq!(az, azure);
    }

    #[test]
    fn azure_storage_target_extracts_account_container_and_prefix() {
        let target = ObjectUrl::parse("az://Account/Container/org/repo")
            .unwrap()
            .azure_storage_target()
            .unwrap();

        assert_eq!(
            target,
            AzureStorageTarget {
                account: "account".to_owned(),
                container: "container".to_owned(),
                object_prefix: "org/repo".to_owned(),
            }
        );
    }

    #[test]
    fn azure_storage_target_allows_container_root() {
        let target = ObjectUrl::parse("az://account/container")
            .unwrap()
            .azure_storage_target()
            .unwrap();

        assert_eq!(target.account, "account");
        assert_eq!(target.container, "container");
        assert_eq!(target.object_prefix, "");
    }

    #[test]
    fn azure_storage_target_rejects_non_azure_or_missing_container() {
        let s3 = ObjectUrl::parse("s3://bucket/repo")
            .unwrap()
            .azure_storage_target()
            .unwrap_err();
        assert!(matches!(s3, UrlError::ExpectedAzureObjectUrl { .. }));

        let missing = ObjectUrl::parse("az://account")
            .unwrap()
            .azure_storage_target()
            .unwrap_err();
        assert!(matches!(missing, UrlError::MissingAzureContainer { .. }));
    }

    #[test]
    fn effective_repo_prefix_uses_raw_or_crab_prefix() {
        for input in ["s3://bucket/org/repo", "crab://bucket/org/repo"] {
            let prefix = ObjectUrl::parse(input)
                .unwrap()
                .effective_repo_prefix("primary/repo")
                .unwrap();
            assert_eq!(prefix, "org/repo");
        }
    }

    #[test]
    fn effective_repo_prefix_uses_default_for_bucket_root() {
        let prefix = ObjectUrl::parse("s3://bucket")
            .unwrap()
            .effective_repo_prefix("primary/repo")
            .unwrap();

        assert_eq!(prefix, "primary/repo");
    }

    #[test]
    fn effective_repo_prefix_strips_azure_container() {
        let nested = ObjectUrl::parse("az://account/container/org/repo")
            .unwrap()
            .effective_repo_prefix("primary/repo")
            .unwrap();
        let root = ObjectUrl::parse("az://account/container")
            .unwrap()
            .effective_repo_prefix("primary/repo")
            .unwrap();

        assert_eq!(nested, "org/repo");
        assert_eq!(root, "primary/repo");
    }

    #[test]
    fn effective_repo_prefix_rejects_azure_without_container() {
        let err = ObjectUrl::parse("az://account")
            .unwrap()
            .effective_repo_prefix("primary/repo")
            .unwrap_err();

        assert!(matches!(err, UrlError::MissingAzureContainer { .. }));
    }

    #[test]
    fn object_url_parses_file_absolute_path() {
        let url = ObjectUrl::parse("file:///tmp/path").unwrap();
        assert_eq!(url.cloud, Cloud::Local);
        assert_eq!(url.bucket, "");
        assert_eq!(url.prefix, "/tmp/path");
    }

    #[test]
    fn object_url_parses_crab_url() {
        let url = ObjectUrl::parse("crab://my-bucket/repos/v2").unwrap();
        assert_eq!(url.form, UrlForm::Crab);
        // V1 default: crab:// resolves to S3 until the config-driven
        // provider resolver lands.
        assert_eq!(url.cloud, Cloud::S3);
        assert_eq!(url.bucket, "my-bucket");
        assert_eq!(url.prefix, "repos/v2");
    }

    #[test]
    fn object_url_bucket_root_has_empty_prefix() {
        // `s3://b/` and `s3://b` should both produce an empty prefix
        // and an equal ObjectUrl value.
        let with_slash = ObjectUrl::parse("s3://b/").unwrap();
        let no_slash = ObjectUrl::parse("s3://b").unwrap();
        assert_eq!(with_slash.bucket, "b");
        assert_eq!(with_slash.prefix, "");
        assert_eq!(with_slash, no_slash);
    }

    #[test]
    fn object_url_trailing_slash_normalized() {
        // Trailing slash on a nested prefix must not produce a
        // different ObjectUrl than the un-slashed form.
        let a = ObjectUrl::parse("s3://b/x/").unwrap();
        let b = ObjectUrl::parse("s3://b/x").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.prefix, "x");
    }

    #[test]
    fn object_url_bucket_and_container_lowercased() {
        // S3/GCS/Azure bucket names are case-insensitive in practice;
        // normalization keeps same-bucket detection honest across
        // inputs that differ only in case.
        let a = ObjectUrl::parse("s3://My-Bucket/PATH").unwrap();
        let b = ObjectUrl::parse("s3://my-bucket/PATH").unwrap();
        assert_eq!(a.bucket, b.bucket);
        // Prefix case is preserved — object keys are case-sensitive.
        assert_eq!(a.prefix, "PATH");
    }

    #[test]
    fn object_url_file_identity_uses_path() {
        // Two `file://` URLs at the same path share an identity; two
        // at different paths do not.
        let a = ObjectUrl::parse("file:///tmp/a").unwrap();
        let b = ObjectUrl::parse("file:///tmp/a/").unwrap();
        let c = ObjectUrl::parse("file:///tmp/b").unwrap();
        assert_eq!(a.bucket_identity(), b.bucket_identity());
        assert_ne!(a.bucket_identity(), c.bucket_identity());
    }

    #[test]
    fn object_url_same_bucket_across_schemes() {
        // The whole point of BucketIdentity: s3:// and crab://
        // pointing at the same underlying bucket compare equal (when
        // crab:// resolves to S3 via the V1 default).
        let raw = ObjectUrl::parse("s3://shared-bucket/data").unwrap();
        let crab = ObjectUrl::parse("crab://shared-bucket/repos/v2").unwrap();
        assert_eq!(raw.bucket_identity(), crab.bucket_identity());
    }

    #[test]
    fn object_url_rejects_https() {
        let err = ObjectUrl::parse("https://foo/bar").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unsupported URL scheme"), "got: {msg}");
    }

    #[test]
    fn object_url_rejects_bare_scheme_separator() {
        // `://` alone has no scheme to classify.
        let err = ObjectUrl::parse("://bucket/path").unwrap_err();
        let msg = err.to_string();
        // Empty scheme string routes through the unknown-scheme arm.
        assert!(
            msg.contains("unsupported URL scheme") || msg.contains("missing scheme"),
            "got: {msg}"
        );
    }

    #[test]
    fn object_url_rejects_missing_separator() {
        let err = ObjectUrl::parse("s3:bucket").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing scheme separator"), "got: {msg}");
    }

    #[test]
    fn object_url_rejects_missing_bucket() {
        let err = ObjectUrl::parse("s3:///path").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing bucket"), "got: {msg}");
    }

    #[test]
    fn object_url_rejects_file_without_path() {
        // `file://` with no path, or `file://host` with nothing after
        // the host, both fail — we need an absolute path.
        let err = ObjectUrl::parse("file://").unwrap_err();
        assert!(err.to_string().contains("missing absolute path"));

        let err = ObjectUrl::parse("file://host").unwrap_err();
        assert!(err.to_string().contains("missing absolute path"));
    }

    #[test]
    fn require_raw_rejects_crab_form() {
        let url = ObjectUrl::parse("crab://my-bucket/repo").unwrap();
        let err = url.require_raw().unwrap_err();
        assert!(
            matches!(err, UrlError::ImportSourceMustBeRaw { .. }),
            "expected ImportSourceMustBeRaw, got {err:?}"
        );
    }

    #[test]
    fn require_raw_accepts_raw_form() {
        for raw in [
            "s3://bucket/path",
            "gs://bucket/path",
            "az://container/path",
            "azure://container/path",
            "file:///tmp/path",
        ] {
            let url = ObjectUrl::parse(raw).unwrap();
            url.require_raw()
                .unwrap_or_else(|e| panic!("require_raw rejected {raw:?}: {e}"));
        }
    }
}
