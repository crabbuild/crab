//! Provider abstraction for lifecycle management and archive restore.
//!
//! Defines the [`LifecycleProvider`] and [`RestoreBackend`] traits that
//! each cloud backend (S3, GCS, Azure) implements. Per-provider
//! implementations live in sibling modules gated behind cargo features
//! (`tier-s3`, `tier-gcs`, `tier-azure`).
//!
//! Types in this module are provider-neutral: they carry the data needed
//! to render, apply, and roll back lifecycle rules, and to orchestrate
//! archive-class restores, without encoding any provider-specific wire
//! format.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::core::error::Result;

#[cfg(feature = "tier-s3")]
pub mod s3;

#[cfg(feature = "tier-gcs")]
pub mod gcs;

#[cfg(feature = "tier-azure")]
pub mod azure;

// Re-export `StorageClass` from the canonical `tier::classes` module so
// that existing `use tier::provider::StorageClass` paths keep working.
pub use super::classes::StorageClass;

// ── Provider-neutral plan + wire types ──────────────────────────────
//
// `TierPlan`, `TierRule`, and `Transition` are the public surface
// [`crate::tier::plan`] builds against. Each provider's `render`
// impl consumes them and emits the matching provider-native
// lifecycle document (S3 XML, GCS / Azure JSON).

/// Provider-neutral lifecycle plan produced by `tier::plan::build`.
///
/// Contains the full set of rules, bucket-level flags, and provider
/// identity needed by each provider's `render` implementation.
#[derive(Debug, Clone)]
pub struct TierPlan {
    /// Target cloud provider.
    pub provider: Provider,
    /// Lifecycle rules to render. Sorted by `id` for deterministic output.
    pub rules: Vec<TierRule>,
    /// Whether the bucket has versioning enabled.
    pub versioning_enabled: bool,
    /// Whether the bucket has object-lock enabled.
    pub object_lock_enabled: bool,
}

/// A single lifecycle rule within a [`TierPlan`].
#[derive(Debug, Clone)]
pub struct TierRule {
    /// Rule identifier, always prefixed `crab-`.
    pub id: String,
    /// Object key prefix this rule applies to (e.g. `.crab/xorbs/`).
    pub prefix: String,
    /// Transitions to apply (Standard → IA, IA → Glacier, etc.).
    pub transitions: Vec<Transition>,
    /// Days after which noncurrent versions expire (versioned buckets).
    pub noncurrent_expiration_days: Option<u32>,
    /// Minimum object size in bytes for the filter. Used to respect
    /// per-class minimums (e.g. S3 Glacier 40 KiB).
    pub min_object_size_bytes: Option<u64>,
}

/// A storage-class transition within a lifecycle rule.
#[derive(Debug, Clone)]
pub struct Transition {
    /// Number of days after object creation before the transition fires.
    pub days: u32,
    /// Target storage class.
    pub to_class: StorageClass,
}

/// Object path within a bucket. Uses the `object_store` path type when
/// available; falls back to a plain `String` for now.
pub type ObjectPath = String;

/// Handle returned by a successful restore submission.
#[derive(Debug, Clone)]
pub struct RestoreHandle {
    /// Provider-assigned restore request identifier.
    pub id: String,
}

// ── Format ──────────────────────────────────────────────────────────

/// Wire format of a rendered lifecycle document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// S3 `PutBucketLifecycleConfiguration` XML.
    Xml,
    /// GCS / Azure JSON.
    Json,
}

// ── Provider ────────────────────────────────────────────────────────

/// Cloud provider that hosts the bucket.
///
/// Detection mirrors the URL-scheme logic already used by `gix-url`
/// throughout the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Provider {
    S3,
    Gcs,
    Azure,
}

impl Provider {
    /// Infer the provider from a store URL scheme.
    ///
    /// Recognises `s3://`, `gs://`, and `az://`. Returns `None` for
    /// unrecognised schemes.
    pub fn from_store_url(url: &str) -> Option<Self> {
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("s3://") {
            Some(Self::S3)
        } else if lower.starts_with("gs://") {
            Some(Self::Gcs)
        } else if lower.starts_with("az://") {
            Some(Self::Azure)
        } else {
            None
        }
    }
}

// ── Guard ───────────────────────────────────────────────────────────

/// CAS guard for conditional lifecycle writes.
///
/// S3 uses ETags, GCS uses generation numbers, and some providers
/// have no CAS mechanism at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guard {
    /// S3 / Azure ETag.
    Etag(String),
    /// GCS generation number.
    Generation(u64),
    /// Provider has no CAS support.
    None,
}

// ── RestoreTier ─────────────────────────────────────────────────────

/// Restore-speed tier for archive-class objects.
///
/// Not every tier is valid for every storage class — see the
/// supported-tier matrix in requirements §A3.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RestoreTier {
    /// S3 Glacier Flexible Retrieval only (~1–5 min).
    Expedited,
    /// S3 Glacier Flexible / Deep Archive, Azure Archive (3–12 h).
    Standard,
    /// S3 Glacier Flexible / Deep Archive (5–48 h).
    Bulk,
    /// Azure Archive high-priority rehydration (<1 h).
    High,
}

// ── RestoreState ────────────────────────────────────────────────────

/// Current restore status of an archived object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreState {
    /// Object is warm or restore has completed.
    Ready,
    /// Restore is in progress.
    InProgress {
        /// When the restore was submitted (RFC 3339 string until chrono
        /// is added as a dependency).
        started_at: String,
        /// Estimated completion time (RFC 3339 string).
        expected_ready_at: String,
    },
    /// Object is cold and no restore has been requested.
    NotRequested,
    /// Restore failed.
    Failed { reason: String, retryable: bool },
}

// ── RenderedLifecycle ───────────────────────────────────────────────

/// A provider-native lifecycle document ready to be applied.
#[derive(Debug, Clone)]
pub struct RenderedLifecycle {
    /// Wire format (XML for S3, JSON for GCS/Azure).
    pub format: Format,
    /// Serialised document body.
    pub body: Vec<u8>,
    /// IDs of the rules contained in this document (all prefixed
    /// `crab-`).
    pub rule_ids: Vec<String>,
}

// ── PutOutcome ──────────────────────────────────────────────────────

/// Result of a successful lifecycle write.
#[derive(Debug, Clone)]
pub struct PutOutcome {
    /// CAS guard returned by the provider for subsequent writes.
    pub new_guard: Guard,
    /// Timestamp when the provider accepted the write (RFC 3339 string
    /// until chrono is added as a dependency).
    pub applied_at: String,
}

// ── HeadMeta ────────────────────────────────────────────────────────

/// Metadata returned by a HEAD / `GetObjectAttributes` call, enriched
/// with storage-class information for the tiering subsystem.
#[derive(Debug, Clone)]
pub struct HeadMeta {
    /// Provider-native storage class.
    pub class: StorageClass,
}

// ── LifecycleProvider trait ─────────────────────────────────────────

/// Manages lifecycle rules for a single bucket on a single provider.
///
/// Implementations live behind per-provider cargo features
/// (`tier-s3`, `tier-gcs`, `tier-azure`).
#[async_trait]
pub trait LifecycleProvider: Send + Sync {
    /// Which cloud provider this implementation targets.
    fn kind(&self) -> Provider;

    /// Emit a provider-native lifecycle document from the plan.
    fn render(&self, plan: &TierPlan) -> Result<RenderedLifecycle>;

    /// Read the current lifecycle configuration. Returns `None` if no
    /// lifecycle is configured on the bucket.
    async fn get(&self) -> Result<Option<RenderedLifecycle>>;

    /// Atomically replace the lifecycle configuration, optionally
    /// guarded by a CAS token.
    async fn put(&self, doc: &RenderedLifecycle, guard: Option<Guard>) -> Result<PutOutcome>;

    /// Remove the lifecycle configuration, optionally guarded by a CAS token.
    async fn delete(&self, _guard: Option<Guard>) -> Result<PutOutcome> {
        Err(crate::core::error::CrabError::TierProviderUnsupported {
            provider: format!("{:?} lifecycle deletion", self.kind()),
        })
    }

    /// Return whether a provider read-back preserves an intended document.
    fn equivalent(
        &self,
        current: &RenderedLifecycle,
        intended: &RenderedLifecycle,
    ) -> Result<bool> {
        Ok(current.format == intended.format && current.body == intended.body)
    }

    /// Fetch the provider-side CAS token for the current lifecycle.
    /// Returns `None` on first write (no existing config).
    async fn cas_guard(&self) -> Result<Option<Guard>>;
}

// ── RestoreBackend trait ────────────────────────────────────────────

/// Submits and monitors archive-class restore requests.
///
/// Implementations live behind per-provider cargo features.
#[async_trait]
pub trait RestoreBackend: Send + Sync {
    /// Submit a restore request for an archived object.
    async fn restore(
        &self,
        path: &ObjectPath,
        tier: RestoreTier,
        duration: Duration,
    ) -> Result<RestoreHandle>;

    /// Query the current restore state of an object.
    async fn state(&self, path: &ObjectPath) -> Result<RestoreState>;

    /// Return the restore tiers supported for the given storage class.
    fn supported_tiers(&self, class: &StorageClass) -> &'static [RestoreTier];
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── from_store_url: recognised schemes ──────────────────────────

    #[test]
    fn from_store_url_s3_with_path() {
        assert_eq!(
            Provider::from_store_url("s3://bucket/path"),
            Some(Provider::S3)
        );
    }

    #[test]
    fn from_store_url_gs_with_path() {
        assert_eq!(
            Provider::from_store_url("gs://bucket/path"),
            Some(Provider::Gcs)
        );
    }

    #[test]
    fn from_store_url_az_with_path() {
        assert_eq!(
            Provider::from_store_url("az://bucket/path"),
            Some(Provider::Azure)
        );
    }

    // ── from_store_url: case-insensitive ────────────────────────────

    #[test]
    fn from_store_url_s3_uppercase() {
        assert_eq!(
            Provider::from_store_url("S3://bucket/path"),
            Some(Provider::S3)
        );
    }

    #[test]
    fn from_store_url_gs_uppercase() {
        assert_eq!(
            Provider::from_store_url("GS://bucket/path"),
            Some(Provider::Gcs)
        );
    }

    #[test]
    fn from_store_url_az_uppercase() {
        assert_eq!(
            Provider::from_store_url("AZ://bucket/path"),
            Some(Provider::Azure)
        );
    }

    #[test]
    fn from_store_url_mixed_case() {
        assert_eq!(
            Provider::from_store_url("S3://Bucket/Path"),
            Some(Provider::S3)
        );
        assert_eq!(Provider::from_store_url("Gs://Bucket"), Some(Provider::Gcs));
        assert_eq!(
            Provider::from_store_url("Az://container"),
            Some(Provider::Azure)
        );
    }

    // ── from_store_url: unknown schemes ─────────────────────────────

    #[test]
    fn from_store_url_http_returns_none() {
        assert_eq!(Provider::from_store_url("http://example.com"), None);
    }

    #[test]
    fn from_store_url_ftp_returns_none() {
        assert_eq!(Provider::from_store_url("ftp://files.example.com"), None);
    }

    #[test]
    fn from_store_url_empty_returns_none() {
        assert_eq!(Provider::from_store_url(""), None);
    }

    #[test]
    fn from_store_url_bare_text_returns_none() {
        assert_eq!(Provider::from_store_url("not-a-url"), None);
    }

    // ── from_store_url: edge cases ──────────────────────────────────

    #[test]
    fn from_store_url_scheme_only_no_path() {
        assert_eq!(Provider::from_store_url("s3://"), Some(Provider::S3));
        assert_eq!(Provider::from_store_url("gs://"), Some(Provider::Gcs));
        assert_eq!(Provider::from_store_url("az://"), Some(Provider::Azure));
    }

    #[test]
    fn from_store_url_scheme_with_trailing_slash() {
        assert_eq!(Provider::from_store_url("s3://bucket/"), Some(Provider::S3));
        assert_eq!(
            Provider::from_store_url("gs://bucket/"),
            Some(Provider::Gcs)
        );
        assert_eq!(
            Provider::from_store_url("az://container/"),
            Some(Provider::Azure)
        );
    }

    #[test]
    fn from_store_url_scheme_prefix_not_enough() {
        // "s3" without "://" should not match.
        assert_eq!(Provider::from_store_url("s3"), None);
        assert_eq!(Provider::from_store_url("s3:"), None);
        assert_eq!(Provider::from_store_url("s3:/"), None);
    }
}
