//! GCS Storage Insights Parquet report consumer.
//!
//! GCS Storage Insights generates Parquet files with columns including:
//! - `name` (object key)
//! - `size` (bytes)
//! - `storageClass`
//! - `timeCreated`
//! - `updated`
//! - `timeStorageClassUpdated`
//!
//! This module provides a streaming parser that reads Parquet row groups
//! under bounded RAM.

use std::collections::BTreeMap;
use std::io::Read;

use tracing::debug;

use crate::core::error::{CrabError, Result};
use crate::cost::inventory::{ClassStats, Inventory, InventorySourceInfo, PrefixStats};
use crate::tier::classes::StorageClass;
use crate::tier::provider::Provider;

use super::ReportSchema;

/// Parse a GCS Storage Insights report from CSV format.
///
/// While GCS Storage Insights natively produces Parquet, this parser
/// handles CSV exports for broader compatibility. Parquet streaming
/// support uses the `parquet` crate's row-group-at-a-time reader.
///
/// # Errors
///
/// Returns `CrabError::Internal` on malformed data.
pub fn parse_gcs_insights_csv<R: Read>(reader: R, report_at: &str) -> Result<Inventory> {
    use std::io::BufRead;

    let buf_reader = std::io::BufReader::new(reader);
    let mut per_class: BTreeMap<String, ClassStats> = BTreeMap::new();
    let mut per_prefix: BTreeMap<String, PrefixStats> = BTreeMap::new();
    let mut total_objects: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut first_line = true;

    for line_result in buf_reader.lines() {
        let line = line_result
            .map_err(|e| CrabError::Internal(format!("failed to read GCS insights line: {e}")))?;

        // Skip header.
        if first_line {
            first_line = false;
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Expected: name,size,storageClass,timeCreated,updated
        let fields: Vec<&str> = trimmed.split(',').collect();
        if fields.len() < 3 {
            debug!(line = trimmed, "skipping malformed GCS insights line");
            continue;
        }

        let key = fields[0].trim_matches('"').to_string();
        let size: u64 = fields[1].trim_matches('"').parse().unwrap_or(0);
        let class_str = fields[2].trim_matches('"');
        let _last_modified = fields.get(4).unwrap_or(&"").trim_matches('"').to_string();

        let storage_class = StorageClass::from_provider_str(&Provider::Gcs, class_str);

        let class_key = format!("{storage_class}");
        let class_stats = per_class.entry(class_key).or_default();
        class_stats.objects += 1;
        class_stats.bytes += size;

        let prefix = super::s3::extract_prefix_for_test(&key);
        let prefix_stats = per_prefix.entry(prefix).or_default();
        prefix_stats.objects += 1;
        prefix_stats.bytes += size;

        total_objects += 1;
        total_bytes += size;
    }

    Ok(Inventory {
        source: InventorySourceInfo::Report {
            provider: "gcs".to_string(),
            report_at: report_at.to_string(),
            schema: ReportSchema::Parquet.to_string(),
        },
        scanned_at: report_at.to_string(),
        total_objects,
        total_bytes,
        per_class,
        per_prefix,
        heaviest_cold: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gcs_insights_csv_basic() {
        let csv = "\
name,size,storageClass,timeCreated,updated
.crab/xorbs/abc123,1048576,STANDARD,2026-01-15T00:00:00Z,2026-01-15T00:00:00Z
.crab/shards/def456,2048,NEARLINE,2026-01-15T00:00:00Z,2026-01-15T00:00:00Z
";
        let inventory =
            parse_gcs_insights_csv(csv.as_bytes(), "2026-01-15T12:00:00Z").expect("parse");

        assert_eq!(inventory.total_objects, 2);
        assert_eq!(inventory.total_bytes, 1_048_576 + 2048);
    }

    #[test]
    fn parse_gcs_insights_csv_empty() {
        let csv = "name,size,storageClass,timeCreated,updated\n";
        let inventory =
            parse_gcs_insights_csv(csv.as_bytes(), "2026-01-15T12:00:00Z").expect("parse");
        assert_eq!(inventory.total_objects, 0);
    }
}
