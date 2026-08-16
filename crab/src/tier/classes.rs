//! Storage-class metadata for all supported cloud providers.
//!
//! Each variant of [`StorageClass`] maps to a provider-native tier and
//! carries the minimum-retention, minimum-object-size, and
//! archive/warm classification needed by the tiering, GC, and hydrate
//! subsystems.

use serde::{Deserialize, Serialize};

use super::provider::Provider;

/// Provider-native storage class.
///
/// Covers S3, GCS, and Azure Blob classes that Crab interacts with.
/// Unrecognised class strings from provider APIs map to [`Unknown`].
///
/// [`Unknown`]: StorageClass::Unknown
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub enum StorageClass {
    // ── AWS S3 ──────────────────────────────────────────────────────
    S3Standard,
    S3IntelligentTiering,
    S3StandardIa,
    S3OneZoneIa,
    S3GlacierInstantRetrieval,
    S3GlacierFlexibleRetrieval,
    S3GlacierDeepArchive,

    // ── Google Cloud Storage ────────────────────────────────────────
    GcsStandard,
    GcsNearline,
    GcsColdline,
    GcsArchive,

    // ── Azure Blob Storage ──────────────────────────────────────────
    AzureHot,
    AzureCool,
    AzureCold,
    AzureArchive,

    // ── Fallback ────────────────────────────────────────────────────
    /// Returned when the provider reports a class string we don't
    /// recognise. Treated as warm with no retention minimum.
    Unknown,
}

impl StorageClass {
    /// Minimum retention period in days before deletion avoids an
    /// early-deletion penalty. Returns `0` for classes with no minimum.
    pub fn min_retention_days(&self) -> u32 {
        match self {
            // S3
            Self::S3Standard | Self::S3IntelligentTiering => 0,
            Self::S3StandardIa | Self::S3OneZoneIa => 30,
            Self::S3GlacierInstantRetrieval | Self::S3GlacierFlexibleRetrieval => 90,
            Self::S3GlacierDeepArchive => 180,

            // GCS
            Self::GcsStandard => 0,
            Self::GcsNearline => 30,
            Self::GcsColdline => 90,
            Self::GcsArchive => 365,

            // Azure
            Self::AzureHot => 0,
            Self::AzureCool => 30,
            Self::AzureCold => 90,
            Self::AzureArchive => 180,

            Self::Unknown => 0,
        }
    }

    /// Minimum billable object size in bytes. Objects smaller than this
    /// are billed as if they were this size.
    ///
    /// S3 Glacier classes: 40 KiB (40 960 bytes).
    /// S3 IA classes: 128 KB (128 000 bytes).
    /// All others: 0 (no minimum).
    pub fn min_object_size_bytes(&self) -> u64 {
        match self {
            Self::S3GlacierInstantRetrieval
            | Self::S3GlacierFlexibleRetrieval
            | Self::S3GlacierDeepArchive => 40_960,

            Self::S3StandardIa | Self::S3OneZoneIa => 128_000,

            _ => 0,
        }
    }

    /// Whether this class requires a `RestoreObject` call before the
    /// object can be read.
    ///
    /// Only classes that truly need a restore step return `true`:
    /// - S3 Glacier Flexible Retrieval
    /// - S3 Glacier Deep Archive
    /// - Azure Archive
    ///
    /// Glacier Instant Retrieval and GCS Archive are **not** archive
    /// classes — they are readable directly (at a retrieval fee).
    pub fn is_archive_class(&self) -> bool {
        matches!(
            self,
            Self::S3GlacierFlexibleRetrieval | Self::S3GlacierDeepArchive | Self::AzureArchive
        )
    }

    /// Whether this class can be read directly without a restore step.
    ///
    /// All non-archive classes are warm. `Unknown` is treated as warm
    /// for backward compatibility (the object is assumed readable).
    pub fn is_warm_class(&self) -> bool {
        !self.is_archive_class()
    }

    /// Map a provider-native class string to a [`StorageClass`].
    ///
    /// Matching is case-insensitive. Unrecognised strings map to
    /// [`StorageClass::Unknown`].
    pub fn from_provider_str(provider: &Provider, s: &str) -> Self {
        let lower = s.to_ascii_lowercase();
        match provider {
            Provider::S3 => Self::from_s3_str(&lower),
            Provider::Gcs => Self::from_gcs_str(&lower),
            Provider::Azure => Self::from_azure_str(&lower),
        }
    }

    fn from_s3_str(s: &str) -> Self {
        match s {
            "standard" => Self::S3Standard,
            "intelligent_tiering" | "intelligent-tiering" => Self::S3IntelligentTiering,
            "standard_ia" | "standard-ia" => Self::S3StandardIa,
            "onezone_ia" | "onezone-ia" => Self::S3OneZoneIa,
            "glacier_ir"
            | "glacier-ir"
            | "glacier_instant_retrieval"
            | "glacier-instant-retrieval" => Self::S3GlacierInstantRetrieval,
            "glacier" | "glacier_flexible_retrieval" | "glacier-flexible-retrieval" => {
                Self::S3GlacierFlexibleRetrieval
            }
            "deep_archive" | "deep-archive" | "glacier_deep_archive" | "glacier-deep-archive" => {
                Self::S3GlacierDeepArchive
            }
            _ => Self::Unknown,
        }
    }

    fn from_gcs_str(s: &str) -> Self {
        match s {
            "standard" => Self::GcsStandard,
            "nearline" => Self::GcsNearline,
            "coldline" => Self::GcsColdline,
            "archive" => Self::GcsArchive,
            _ => Self::Unknown,
        }
    }

    fn from_azure_str(s: &str) -> Self {
        match s {
            "hot" => Self::AzureHot,
            "cool" => Self::AzureCool,
            "cold" => Self::AzureCold,
            "archive" => Self::AzureArchive,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for StorageClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::S3Standard => write!(f, "S3 Standard"),
            Self::S3IntelligentTiering => write!(f, "S3 Intelligent-Tiering"),
            Self::S3StandardIa => write!(f, "S3 Standard-IA"),
            Self::S3OneZoneIa => write!(f, "S3 One Zone-IA"),
            Self::S3GlacierInstantRetrieval => write!(f, "S3 Glacier Instant Retrieval"),
            Self::S3GlacierFlexibleRetrieval => write!(f, "S3 Glacier Flexible Retrieval"),
            Self::S3GlacierDeepArchive => write!(f, "S3 Glacier Deep Archive"),
            Self::GcsStandard => write!(f, "GCS Standard"),
            Self::GcsNearline => write!(f, "GCS Nearline"),
            Self::GcsColdline => write!(f, "GCS Coldline"),
            Self::GcsArchive => write!(f, "GCS Archive"),
            Self::AzureHot => write!(f, "Azure Hot"),
            Self::AzureCool => write!(f, "Azure Cool"),
            Self::AzureCold => write!(f, "Azure Cold"),
            Self::AzureArchive => write!(f, "Azure Archive"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── min_retention_days ──────────────────────────────────────────

    #[test]
    fn min_retention_days_s3_classes() {
        assert_eq!(StorageClass::S3Standard.min_retention_days(), 0);
        assert_eq!(StorageClass::S3IntelligentTiering.min_retention_days(), 0);
        assert_eq!(StorageClass::S3StandardIa.min_retention_days(), 30);
        assert_eq!(StorageClass::S3OneZoneIa.min_retention_days(), 30);
        assert_eq!(
            StorageClass::S3GlacierInstantRetrieval.min_retention_days(),
            90
        );
        assert_eq!(
            StorageClass::S3GlacierFlexibleRetrieval.min_retention_days(),
            90
        );
        assert_eq!(StorageClass::S3GlacierDeepArchive.min_retention_days(), 180);
    }

    #[test]
    fn min_retention_days_gcs_classes() {
        assert_eq!(StorageClass::GcsStandard.min_retention_days(), 0);
        assert_eq!(StorageClass::GcsNearline.min_retention_days(), 30);
        assert_eq!(StorageClass::GcsColdline.min_retention_days(), 90);
        assert_eq!(StorageClass::GcsArchive.min_retention_days(), 365);
    }

    #[test]
    fn min_retention_days_azure_classes() {
        assert_eq!(StorageClass::AzureHot.min_retention_days(), 0);
        assert_eq!(StorageClass::AzureCool.min_retention_days(), 30);
        assert_eq!(StorageClass::AzureCold.min_retention_days(), 90);
        assert_eq!(StorageClass::AzureArchive.min_retention_days(), 180);
    }

    #[test]
    fn min_retention_days_unknown_is_zero() {
        assert_eq!(StorageClass::Unknown.min_retention_days(), 0);
    }

    // ── min_object_size_bytes ───────────────────────────────────────

    #[test]
    fn min_object_size_s3_glacier_classes() {
        assert_eq!(
            StorageClass::S3GlacierInstantRetrieval.min_object_size_bytes(),
            40_960
        );
        assert_eq!(
            StorageClass::S3GlacierFlexibleRetrieval.min_object_size_bytes(),
            40_960
        );
        assert_eq!(
            StorageClass::S3GlacierDeepArchive.min_object_size_bytes(),
            40_960
        );
    }

    #[test]
    fn min_object_size_s3_ia_classes() {
        assert_eq!(StorageClass::S3StandardIa.min_object_size_bytes(), 128_000);
        assert_eq!(StorageClass::S3OneZoneIa.min_object_size_bytes(), 128_000);
    }

    #[test]
    fn min_object_size_zero_for_standard_and_it() {
        assert_eq!(StorageClass::S3Standard.min_object_size_bytes(), 0);
        assert_eq!(
            StorageClass::S3IntelligentTiering.min_object_size_bytes(),
            0
        );
    }

    #[test]
    fn min_object_size_zero_for_gcs() {
        assert_eq!(StorageClass::GcsStandard.min_object_size_bytes(), 0);
        assert_eq!(StorageClass::GcsNearline.min_object_size_bytes(), 0);
        assert_eq!(StorageClass::GcsColdline.min_object_size_bytes(), 0);
        assert_eq!(StorageClass::GcsArchive.min_object_size_bytes(), 0);
    }

    #[test]
    fn min_object_size_zero_for_azure() {
        assert_eq!(StorageClass::AzureHot.min_object_size_bytes(), 0);
        assert_eq!(StorageClass::AzureCool.min_object_size_bytes(), 0);
        assert_eq!(StorageClass::AzureCold.min_object_size_bytes(), 0);
        assert_eq!(StorageClass::AzureArchive.min_object_size_bytes(), 0);
    }

    #[test]
    fn min_object_size_unknown_is_zero() {
        assert_eq!(StorageClass::Unknown.min_object_size_bytes(), 0);
    }

    // ── is_archive_class ────────────────────────────────────────────

    #[test]
    fn archive_classes_require_restore() {
        assert!(StorageClass::S3GlacierFlexibleRetrieval.is_archive_class());
        assert!(StorageClass::S3GlacierDeepArchive.is_archive_class());
        assert!(StorageClass::AzureArchive.is_archive_class());
    }

    #[test]
    fn glacier_instant_retrieval_is_not_archive() {
        assert!(!StorageClass::S3GlacierInstantRetrieval.is_archive_class());
    }

    #[test]
    fn gcs_archive_is_not_archive_class() {
        assert!(!StorageClass::GcsArchive.is_archive_class());
    }

    #[test]
    fn non_archive_classes() {
        let non_archive = [
            StorageClass::S3Standard,
            StorageClass::S3IntelligentTiering,
            StorageClass::S3StandardIa,
            StorageClass::S3OneZoneIa,
            StorageClass::S3GlacierInstantRetrieval,
            StorageClass::GcsStandard,
            StorageClass::GcsNearline,
            StorageClass::GcsColdline,
            StorageClass::GcsArchive,
            StorageClass::AzureHot,
            StorageClass::AzureCool,
            StorageClass::AzureCold,
            StorageClass::Unknown,
        ];
        for class in non_archive {
            assert!(
                !class.is_archive_class(),
                "{class:?} should not be an archive class"
            );
        }
    }

    // ── is_warm_class ───────────────────────────────────────────────

    #[test]
    fn warm_classes_readable_directly() {
        let warm = [
            StorageClass::S3Standard,
            StorageClass::S3IntelligentTiering,
            StorageClass::S3StandardIa,
            StorageClass::S3OneZoneIa,
            StorageClass::S3GlacierInstantRetrieval,
            StorageClass::GcsStandard,
            StorageClass::GcsNearline,
            StorageClass::GcsColdline,
            StorageClass::GcsArchive,
            StorageClass::AzureHot,
            StorageClass::AzureCool,
            StorageClass::AzureCold,
            StorageClass::Unknown,
        ];
        for class in warm {
            assert!(class.is_warm_class(), "{class:?} should be a warm class");
        }
    }

    #[test]
    fn archive_classes_are_not_warm() {
        assert!(!StorageClass::S3GlacierFlexibleRetrieval.is_warm_class());
        assert!(!StorageClass::S3GlacierDeepArchive.is_warm_class());
        assert!(!StorageClass::AzureArchive.is_warm_class());
    }

    // ── from_provider_str: S3 ───────────────────────────────────────

    #[test]
    fn from_provider_str_s3_standard() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "STANDARD"),
            StorageClass::S3Standard
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "standard"),
            StorageClass::S3Standard
        );
    }

    #[test]
    fn from_provider_str_s3_intelligent_tiering() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "INTELLIGENT_TIERING"),
            StorageClass::S3IntelligentTiering
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "intelligent-tiering"),
            StorageClass::S3IntelligentTiering
        );
    }

    #[test]
    fn from_provider_str_s3_standard_ia() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "STANDARD_IA"),
            StorageClass::S3StandardIa
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "standard-ia"),
            StorageClass::S3StandardIa
        );
    }

    #[test]
    fn from_provider_str_s3_onezone_ia() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "ONEZONE_IA"),
            StorageClass::S3OneZoneIa
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "onezone-ia"),
            StorageClass::S3OneZoneIa
        );
    }

    #[test]
    fn from_provider_str_s3_glacier_instant_retrieval() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "GLACIER_IR"),
            StorageClass::S3GlacierInstantRetrieval
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "glacier-ir"),
            StorageClass::S3GlacierInstantRetrieval
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "GLACIER_INSTANT_RETRIEVAL"),
            StorageClass::S3GlacierInstantRetrieval
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "glacier-instant-retrieval"),
            StorageClass::S3GlacierInstantRetrieval
        );
    }

    #[test]
    fn from_provider_str_s3_glacier_flexible_retrieval() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "GLACIER"),
            StorageClass::S3GlacierFlexibleRetrieval
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "GLACIER_FLEXIBLE_RETRIEVAL"),
            StorageClass::S3GlacierFlexibleRetrieval
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "glacier-flexible-retrieval"),
            StorageClass::S3GlacierFlexibleRetrieval
        );
    }

    #[test]
    fn from_provider_str_s3_glacier_deep_archive() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "DEEP_ARCHIVE"),
            StorageClass::S3GlacierDeepArchive
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "deep-archive"),
            StorageClass::S3GlacierDeepArchive
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "GLACIER_DEEP_ARCHIVE"),
            StorageClass::S3GlacierDeepArchive
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "glacier-deep-archive"),
            StorageClass::S3GlacierDeepArchive
        );
    }

    // ── from_provider_str: GCS ──────────────────────────────────────

    #[test]
    fn from_provider_str_gcs_standard() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Gcs, "STANDARD"),
            StorageClass::GcsStandard
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Gcs, "standard"),
            StorageClass::GcsStandard
        );
    }

    #[test]
    fn from_provider_str_gcs_nearline() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Gcs, "NEARLINE"),
            StorageClass::GcsNearline
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Gcs, "nearline"),
            StorageClass::GcsNearline
        );
    }

    #[test]
    fn from_provider_str_gcs_coldline() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Gcs, "COLDLINE"),
            StorageClass::GcsColdline
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Gcs, "coldline"),
            StorageClass::GcsColdline
        );
    }

    #[test]
    fn from_provider_str_gcs_archive() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Gcs, "ARCHIVE"),
            StorageClass::GcsArchive
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Gcs, "archive"),
            StorageClass::GcsArchive
        );
    }

    // ── from_provider_str: Azure ────────────────────────────────────

    #[test]
    fn from_provider_str_azure_hot() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Azure, "HOT"),
            StorageClass::AzureHot
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Azure, "hot"),
            StorageClass::AzureHot
        );
    }

    #[test]
    fn from_provider_str_azure_cool() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Azure, "COOL"),
            StorageClass::AzureCool
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Azure, "cool"),
            StorageClass::AzureCool
        );
    }

    #[test]
    fn from_provider_str_azure_cold() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Azure, "COLD"),
            StorageClass::AzureCold
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Azure, "cold"),
            StorageClass::AzureCold
        );
    }

    #[test]
    fn from_provider_str_azure_archive() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Azure, "ARCHIVE"),
            StorageClass::AzureArchive
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Azure, "archive"),
            StorageClass::AzureArchive
        );
    }

    // ── from_provider_str: Unknown ──────────────────────────────────

    #[test]
    fn from_provider_str_unknown_s3() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "REDUCED_REDUNDANCY"),
            StorageClass::Unknown
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, ""),
            StorageClass::Unknown
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::S3, "not-a-class"),
            StorageClass::Unknown
        );
    }

    #[test]
    fn from_provider_str_unknown_gcs() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Gcs, "DURABLE_REDUCED_AVAILABILITY"),
            StorageClass::Unknown
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Gcs, ""),
            StorageClass::Unknown
        );
    }

    #[test]
    fn from_provider_str_unknown_azure() {
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Azure, "premium"),
            StorageClass::Unknown
        );
        assert_eq!(
            StorageClass::from_provider_str(&Provider::Azure, ""),
            StorageClass::Unknown
        );
    }

    // ── warm / archive are complementary ────────────────────────────

    #[test]
    fn warm_and_archive_are_complementary() {
        let all = [
            StorageClass::S3Standard,
            StorageClass::S3IntelligentTiering,
            StorageClass::S3StandardIa,
            StorageClass::S3OneZoneIa,
            StorageClass::S3GlacierInstantRetrieval,
            StorageClass::S3GlacierFlexibleRetrieval,
            StorageClass::S3GlacierDeepArchive,
            StorageClass::GcsStandard,
            StorageClass::GcsNearline,
            StorageClass::GcsColdline,
            StorageClass::GcsArchive,
            StorageClass::AzureHot,
            StorageClass::AzureCool,
            StorageClass::AzureCold,
            StorageClass::AzureArchive,
            StorageClass::Unknown,
        ];
        for class in all {
            assert_ne!(
                class.is_archive_class(),
                class.is_warm_class(),
                "{class:?} must be exactly one of archive or warm"
            );
        }
    }
}
