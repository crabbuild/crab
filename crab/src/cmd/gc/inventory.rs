//! Fail-closed contracts for optional provider inventory candidates.
//!
//! Cost inventory readers intentionally remain permissive because estimates
//! can tolerate incomplete rows. Destructive GC must use this separate
//! contract: a report is executable only when its scope, freshness,
//! completeness, and object identity are proven before the first batch.

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::core::error::{CrabError, Result};

pub const INVENTORY_SCHEMA_VERSION: u32 = 1;
pub const MAX_STRICT_INVENTORY_ROWS_IN_MEMORY: usize = 16_384;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrictInventoryManifest {
    pub schema_version: u32,
    pub provider: String,
    pub bucket: String,
    pub prefix: String,
    pub report_id: String,
    pub generated_at_unix_ms: i64,
    pub member_count: u64,
    pub row_count: u64,
    pub digest: String,
    pub complete: bool,
    pub schema: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StrictInventoryRow {
    pub key: String,
    pub size: u64,
    pub last_modified_unix_ms: i64,
    pub etag: Option<String>,
    pub version: Option<String>,
    pub delete_marker: bool,
}

#[derive(Debug, Clone)]
pub struct StrictInventoryContract {
    pub manifest: StrictInventoryManifest,
    pub max_staleness: Duration,
    pub now: SystemTime,
}

impl StrictInventoryContract {
    /// Validates the report identity before any candidate row is accepted.
    pub fn new(
        manifest: StrictInventoryManifest,
        provider: &str,
        bucket: &str,
        prefix: &str,
        max_staleness: Duration,
        now: SystemTime,
    ) -> Result<Self> {
        if manifest.schema_version != INVENTORY_SCHEMA_VERSION {
            return Err(inventory_configuration(
                "schema_version",
                format!(
                    "unsupported inventory schema {}; expected {}",
                    manifest.schema_version, INVENTORY_SCHEMA_VERSION
                ),
            ));
        }
        if manifest.provider != provider || manifest.bucket != bucket || manifest.prefix != prefix {
            return Err(inventory_configuration(
                "scope",
                "inventory provider, bucket, or prefix does not match the GC scope",
            ));
        }
        if prefix.is_empty() || !prefix.ends_with('/') {
            return Err(inventory_configuration(
                "scope",
                "inventory prefix must be a non-empty canonical path ending with '/'",
            ));
        }
        if manifest.report_id.trim().is_empty()
            || manifest.schema.trim().is_empty()
            || manifest.digest.len() != 64
            || !manifest
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(inventory_configuration(
                "identity",
                "inventory report id, digest, and schema are required",
            ));
        }
        if !manifest.complete {
            return Err(inventory_configuration(
                "complete",
                "incomplete provider inventory cannot authorize destructive GC",
            ));
        }
        let generated_at = unix_ms_to_time(manifest.generated_at_unix_ms)?;
        let age = now.duration_since(generated_at).map_err(|_| {
            inventory_configuration(
                "generated_at",
                "inventory report generation time is in the future",
            )
        })?;
        if age > max_staleness {
            return Err(inventory_configuration(
                "freshness",
                format!("inventory report is {age:?} old; maximum is {max_staleness:?}"),
            ));
        }
        Ok(Self {
            manifest,
            max_staleness,
            now,
        })
    }

    /// Validates one row against the pinned scope and object identity rules.
    pub fn validate_row(&self, row: &StrictInventoryRow) -> Result<()> {
        if row.key.is_empty()
            || row.key.contains('\0')
            || row
                .key
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
            || !row.key.starts_with(&self.manifest.prefix)
        {
            return Err(inventory_corrupt(
                &row.key,
                "inventory row is outside the pinned GC prefix",
            ));
        }
        let modified = unix_ms_to_time(row.last_modified_unix_ms)?;
        if modified > self.now {
            return Err(inventory_corrupt(
                &row.key,
                "inventory row modification time is in the future",
            ));
        }
        if row.delete_marker {
            return Err(inventory_corrupt(
                &row.key,
                "delete-marker rows cannot authorize object deletion",
            ));
        }
        if row
            .etag
            .as_deref()
            .is_none_or(|etag| etag.trim().is_empty())
            && row
                .version
                .as_deref()
                .is_none_or(|version| version.trim().is_empty())
        {
            return Err(inventory_configuration(
                "object_identity",
                format!(
                    "inventory row {} has no stable ETag or version identity",
                    row.key
                ),
            ));
        }
        Ok(())
    }
}

/// Bounded strict-row accumulator used by adapters that cannot expose a
/// provider-native async stream. Exceeding the cap fails closed instead of
/// allowing a report parser to grow without bound.
#[derive(Debug, Default)]
pub struct StrictInventoryRows {
    rows: Vec<StrictInventoryRow>,
    identities: HashSet<String>,
}

impl StrictInventoryRows {
    pub fn push(
        &mut self,
        contract: &StrictInventoryContract,
        row: StrictInventoryRow,
    ) -> Result<()> {
        contract.validate_row(&row)?;
        if self.rows.len() >= MAX_STRICT_INVENTORY_ROWS_IN_MEMORY {
            return Err(inventory_configuration(
                "memory_budget",
                "strict inventory adapter must stream rows into bounded GC batches",
            ));
        }
        let identity = format!(
            "{}\0{}\0{}",
            row.key,
            row.version
                .as_deref()
                .or(row.etag.as_deref())
                .unwrap_or_default(),
            row.delete_marker
        );
        if !self.identities.insert(identity) {
            return Err(inventory_corrupt(
                &row.key,
                "duplicate inventory object identity",
            ));
        }
        self.rows.push(row);
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Drains one bounded batch. Duplicate identity detection is scoped to
    /// this accumulator; a provider adapter that spans batches must carry its
    /// identity digest in the durable report manifest before GC can consume it.
    pub fn drain(&mut self) -> impl Iterator<Item = StrictInventoryRow> + '_ {
        self.identities.clear();
        self.rows.drain(..)
    }
}

/// Current cost-report adapters are deliberately not destructive sources.
pub fn require_destructive_provider_support(provider: &str, schema: &str) -> Result<()> {
    Err(inventory_configuration(
        "source",
        format!(
            "provider inventory source {provider}/{schema} is not destructive-GC qualified; use live listing"
        ),
    ))
}

fn unix_ms_to_time(value: i64) -> Result<SystemTime> {
    let millis = u64::try_from(value)
        .map_err(|_| inventory_corrupt("inventory manifest", "timestamp is negative"))?;
    UNIX_EPOCH
        .checked_add(Duration::from_millis(millis))
        .ok_or_else(|| inventory_corrupt("inventory manifest", "timestamp overflows"))
}

fn inventory_configuration(key: &str, origin: impl Into<String>) -> CrabError {
    CrabError::Configuration {
        key: format!("gc.inventory.{key}"),
        origin: origin.into(),
    }
}

fn inventory_corrupt(path: &str, reason: impl Into<String>) -> CrabError {
    CrabError::CorruptObject {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> StrictInventoryManifest {
        StrictInventoryManifest {
            schema_version: INVENTORY_SCHEMA_VERSION,
            provider: "s3".to_owned(),
            bucket: "bucket".to_owned(),
            prefix: ".crab/xorbs/".to_owned(),
            report_id: "report-1".to_owned(),
            generated_at_unix_ms: 1_000,
            member_count: 1,
            row_count: 1,
            digest: "d".repeat(64),
            complete: true,
            schema: "strict-csv-v1".to_owned(),
        }
    }

    fn row() -> StrictInventoryRow {
        StrictInventoryRow {
            key: ".crab/xorbs/aa/hash".to_owned(),
            size: 10,
            last_modified_unix_ms: 900,
            etag: Some("etag".to_owned()),
            version: None,
            delete_marker: false,
        }
    }

    #[test]
    fn strict_contract_accepts_matching_complete_report() {
        let contract = StrictInventoryContract::new(
            manifest(),
            "s3",
            "bucket",
            ".crab/xorbs/",
            Duration::from_secs(10),
            UNIX_EPOCH + Duration::from_millis(2_000),
        )
        .unwrap();
        contract.validate_row(&row()).unwrap();
    }

    #[test]
    fn strict_contract_rejects_incomplete_or_stale_report() {
        let mut incomplete = manifest();
        incomplete.complete = false;
        assert!(
            StrictInventoryContract::new(
                incomplete,
                "s3",
                "bucket",
                ".crab/xorbs/",
                Duration::from_secs(10),
                UNIX_EPOCH + Duration::from_millis(2_000),
            )
            .is_err()
        );

        let stale = StrictInventoryContract::new(
            manifest(),
            "s3",
            "bucket",
            ".crab/xorbs/",
            Duration::from_millis(500),
            UNIX_EPOCH + Duration::from_millis(2_000),
        );
        assert!(stale.is_err());
    }

    #[test]
    fn strict_contract_rejects_scope_identity_and_delete_markers() {
        let contract = StrictInventoryContract::new(
            manifest(),
            "s3",
            "bucket",
            ".crab/xorbs/",
            Duration::from_secs(10),
            UNIX_EPOCH + Duration::from_millis(2_000),
        )
        .unwrap();
        let mut outside = row();
        outside.key = ".crab/shards/aa/hash".to_owned();
        assert!(contract.validate_row(&outside).is_err());
        let mut marker = row();
        marker.delete_marker = true;
        assert!(contract.validate_row(&marker).is_err());
        let mut identity = row();
        identity.etag = None;
        assert!(contract.validate_row(&identity).is_err());
    }

    #[test]
    fn strict_rows_reject_duplicate_identity() {
        let contract = StrictInventoryContract::new(
            manifest(),
            "s3",
            "bucket",
            ".crab/xorbs/",
            Duration::from_secs(10),
            UNIX_EPOCH + Duration::from_millis(2_000),
        )
        .unwrap();
        let mut rows = StrictInventoryRows::default();
        rows.push(&contract, row()).unwrap();
        assert!(rows.push(&contract, row()).is_err());
    }

    #[test]
    fn cost_inventory_source_is_not_destructive() {
        assert!(require_destructive_provider_support("s3", "csv").is_err());
    }
}
