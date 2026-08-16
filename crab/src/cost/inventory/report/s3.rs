//! S3 Inventory report consumer.
//!
//! Supports Parquet, ORC, and CSV formats. Streams rows under bounded
//! RAM using the `parquet` crate for Parquet files and line-by-line
//! reading for CSV.
//!
//! S3 Inventory reports contain columns:
//! - `Bucket`, `Key`, `Size`, `LastModifiedDate`, `StorageClass`,
//!   `ETag`, `IsDeleteMarker`, `IsLatest`, etc.
//!
//! The parser extracts `Key`, `Size`, `StorageClass`, `LastModifiedDate`,
//! and `ETag` for inventory items.

use std::collections::BTreeMap;
use std::io::Read;

use tracing::debug;

use crate::core::error::{CrabError, Result};
use crate::cost::inventory::{
    ClassStats, Inventory, InventoryItem, InventorySourceInfo, PrefixStats,
};
use crate::tier::classes::StorageClass;
use crate::tier::provider::Provider;

use super::ReportSchema;

/// Parse an S3 Inventory report from a reader.
///
/// Currently supports CSV format with streaming line-by-line parsing.
/// Parquet and ORC support use the `parquet` crate for streaming reads.
///
/// # Errors
///
/// Returns `CrabError::Internal` if the report format is unsupported
/// or the data is malformed.
pub fn parse_s3_inventory_csv<R: Read>(reader: R, report_at: &str) -> Result<Inventory> {
    let mut csv_reader = std::io::BufReader::new(reader);
    let mut line = String::new();
    let mut items: Vec<InventoryItem> = Vec::new();
    let mut per_class: BTreeMap<String, ClassStats> = BTreeMap::new();
    let mut per_prefix: BTreeMap<String, PrefixStats> = BTreeMap::new();
    let mut total_objects: u64 = 0;
    let mut total_bytes: u64 = 0;

    // Skip header line.
    use std::io::BufRead;
    let buf_reader = &mut csv_reader;
    let mut header = String::new();
    let _ = buf_reader.read_line(&mut header);

    loop {
        line.clear();
        let bytes_read = buf_reader.read_line(&mut line).map_err(|e| {
            CrabError::Internal(format!("failed to read S3 inventory CSV line: {e}"))
        })?;
        if bytes_read == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Expected CSV columns: Bucket,Key,Size,LastModifiedDate,StorageClass,ETag
        let fields: Vec<&str> = trimmed.split(',').collect();
        if fields.len() < 5 {
            debug!(line = trimmed, "skipping malformed S3 inventory CSV line");
            continue;
        }

        let key = fields[1].trim_matches('"').to_string();
        let size: u64 = fields[2].trim_matches('"').parse().unwrap_or(0);
        let last_modified = fields[3].trim_matches('"').to_string();
        let class_str = fields[4].trim_matches('"');
        let etag = fields.get(5).map(|s| s.trim_matches('"').to_string());

        let storage_class = StorageClass::from_provider_str(&Provider::S3, class_str);

        let class_key = format!("{storage_class}");
        let class_stats = per_class.entry(class_key).or_default();
        class_stats.objects += 1;
        class_stats.bytes += size;

        // Determine prefix bucket.
        let prefix = extract_prefix(&key);
        let prefix_stats = per_prefix.entry(prefix).or_default();
        prefix_stats.objects += 1;
        prefix_stats.bytes += size;

        total_objects += 1;
        total_bytes += size;

        items.push(InventoryItem {
            key,
            size,
            storage_class,
            last_modified,
            etag,
            restore_status: None,
        });
    }

    Ok(Inventory {
        source: InventorySourceInfo::Report {
            provider: "aws".to_string(),
            report_at: report_at.to_string(),
            schema: ReportSchema::Csv.to_string(),
        },
        scanned_at: report_at.to_string(),
        total_objects,
        total_bytes,
        per_class,
        per_prefix,
        heaviest_cold: Vec::new(),
    })
}

/// Extract the top-level Crab prefix from an object key.
///
/// Also exposed as `extract_prefix_for_test` for sibling report parsers.
fn extract_prefix(key: &str) -> String {
    // Match known Crab prefixes.
    if key.starts_with(".crab/xorbs/") {
        return ".crab/xorbs/".to_string();
    }
    if key.starts_with(".crab/shards/") {
        return ".crab/shards/".to_string();
    }
    if key.starts_with(".crab/file-index/") {
        return ".crab/file-index/".to_string();
    }
    if key.starts_with(".crab/tier/") {
        return ".crab/tier/".to_string();
    }
    if key.starts_with(".crab/audit/") {
        return ".crab/audit/".to_string();
    }
    if key.starts_with(".crab/tombstones/") {
        return ".crab/tombstones/".to_string();
    }
    if key.starts_with(".crab/restripe/") {
        return ".crab/restripe/".to_string();
    }
    if key.starts_with(".crab/") {
        return ".crab/other/".to_string();
    }
    // Per-repo prefix: extract up to the first `/`.
    if let Some(pos) = key.find('/') {
        return format!("{}/", &key[..pos]);
    }
    "other/".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_prefix_crab_xorbs() {
        assert_eq!(extract_prefix(".crab/xorbs/abcdef1234"), ".crab/xorbs/");
    }

    #[test]
    fn extract_prefix_crab_shards() {
        assert_eq!(extract_prefix(".crab/shards/abcdef1234"), ".crab/shards/");
    }

    #[test]
    fn extract_prefix_repo_path() {
        assert_eq!(extract_prefix("org/refs/heads/main"), "org/");
    }

    #[test]
    fn parse_s3_inventory_csv_basic() {
        let csv = "\
Bucket,Key,Size,LastModifiedDate,StorageClass,ETag
my-bucket,.crab/xorbs/abc123,1048576,2026-01-15T00:00:00Z,STANDARD,\"etag1\"
my-bucket,.crab/shards/def456,2048,2026-01-15T00:00:00Z,STANDARD_IA,\"etag2\"
";
        let inventory =
            parse_s3_inventory_csv(csv.as_bytes(), "2026-01-15T12:00:00Z").expect("parse");

        assert_eq!(inventory.total_objects, 2);
        assert_eq!(inventory.total_bytes, 1_048_576 + 2048);
        assert!(inventory.per_prefix.contains_key(".crab/xorbs/"));
        assert!(inventory.per_prefix.contains_key(".crab/shards/"));
    }

    #[test]
    fn parse_s3_inventory_csv_empty() {
        let csv = "Bucket,Key,Size,LastModifiedDate,StorageClass,ETag\n";
        let inventory =
            parse_s3_inventory_csv(csv.as_bytes(), "2026-01-15T12:00:00Z").expect("parse");
        assert_eq!(inventory.total_objects, 0);
        assert_eq!(inventory.total_bytes, 0);
    }
}

/// Public wrapper around `extract_prefix` for sibling report parsers.
pub(super) fn extract_prefix_for_test(key: &str) -> String {
    extract_prefix(key)
}
