//! Cache-service wire contracts that do not require an HTTP client.

use std::ops::Range;

use bytes::Bytes;

use crate::path_class::CacheRouteContract;

/// Result of a batch dedup query against the cache service.
///
/// Mirrors the JSON response from `POST /v1/dedup/query`.
#[derive(Debug, serde::Deserialize)]
pub struct DedupQueryResult {
    /// Chunks found in the cache service's index.
    pub known: Vec<KnownChunk>,
    /// Indices of chunks not found in the index.
    pub unknown: Vec<usize>,
}

/// A chunk found in the dedup index with its xorb location.
#[derive(Debug, serde::Deserialize)]
pub struct KnownChunk {
    pub index: usize,
    pub xorb_hash: String,
    pub chunk_index: u32,
    pub length: u32,
    #[serde(default)]
    pub cache_verified: bool,
}

/// Metadata returned by `HEAD /v1/{path}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheObjectHead {
    pub size: u64,
    pub cache_status: Option<String>,
}

/// Byte-range body plus cache-status metadata from `GET /v1/{path}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheObjectRange {
    pub data: Bytes,
    pub range: Range<u64>,
    pub total_size: u64,
    pub cache_status: Option<String>,
}

/// Cache-service capabilities used by clients to avoid unsupported requests.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CacheServiceCapabilities {
    pub limits: CacheServiceLimits,
    #[serde(default)]
    pub routes: Option<CacheRouteContract>,
}

/// Cache-service request and storage limits.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CacheServiceLimits {
    pub max_cache_bytes: u64,
    pub max_object_bytes: u64,
}

/// Client mode for optional cache-service acceleration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheServiceMode {
    /// Route immutable reads through the cache service.
    Cache,
    /// Query the cache service for chunk dedup during push.
    Dedup,
    /// Use both cache reads and dedup queries.
    CacheAndDedup,
}

impl CacheServiceMode {
    /// Return the config string for this cache-service mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Dedup => "dedup",
            Self::CacheAndDedup => "cache+dedup",
        }
    }

    /// Return true when this mode enables cache-service reads.
    #[must_use]
    pub fn cache_reads_enabled(self) -> bool {
        matches!(self, Self::Cache | Self::CacheAndDedup)
    }

    /// Return true when this mode enables cache-service dedup queries.
    #[must_use]
    pub fn dedup_enabled(self) -> bool {
        matches!(self, Self::Dedup | Self::CacheAndDedup)
    }
}

impl std::str::FromStr for CacheServiceMode {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "cache" => Ok(Self::Cache),
            "dedup" => Ok(Self::Dedup),
            "cache+dedup" => Ok(Self::CacheAndDedup),
            _ => Err(()),
        }
    }
}

/// Authentication mode for the cache service client.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CacheServiceAuth {
    /// No authentication.
    #[default]
    None,
    /// Pre-shared key sent via `X-Cache-PSK`.
    Psk(String),
    /// Bearer token sent via `Authorization`.
    Bearer(String),
    /// mTLS client identity with no additional HTTP auth header.
    Mtls,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_service_mode_parses_config_values() {
        assert_eq!(
            "cache".parse::<CacheServiceMode>(),
            Ok(CacheServiceMode::Cache)
        );
        assert_eq!(
            "dedup".parse::<CacheServiceMode>(),
            Ok(CacheServiceMode::Dedup)
        );
        assert_eq!(
            "cache+dedup".parse::<CacheServiceMode>(),
            Ok(CacheServiceMode::CacheAndDedup)
        );
        assert!("both".parse::<CacheServiceMode>().is_err());
    }

    #[test]
    fn cache_service_mode_reports_enabled_legs() {
        assert!(CacheServiceMode::Cache.cache_reads_enabled());
        assert!(!CacheServiceMode::Cache.dedup_enabled());

        assert!(!CacheServiceMode::Dedup.cache_reads_enabled());
        assert!(CacheServiceMode::Dedup.dedup_enabled());

        assert!(CacheServiceMode::CacheAndDedup.cache_reads_enabled());
        assert!(CacheServiceMode::CacheAndDedup.dedup_enabled());
        assert_eq!(CacheServiceMode::CacheAndDedup.as_str(), "cache+dedup");
    }
}
