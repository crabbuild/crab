//! LFS object lifecycle management.
//!
//! Provides prune (identify/delete unreferenced objects), fsck (verify
//! object integrity), and lifecycle-policy generation for cloud providers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::error::{CrabError, Result};
use crab_lfs::{LfsError, LfsObjectStore};

/// Run prune: identify LFS objects not referenced by any reachable commit.
///
/// In `dry_run` mode, only reports unreferenced objects. In `delete` mode,
/// deletes them after confirmation. `retain_days` preserves objects younger
/// than N days even if unreferenced. `since` optionally limits traversal
/// to commits newer than a date.
pub async fn run_prune(
    store: &LfsObjectStore,
    dry_run: bool,
    delete: bool,
    retain_days: u32,
    since: Option<&str>,
) -> Result<PruneReport> {
    // Collect referenced OIDs from git.
    let referenced = collect_referenced_oids(since)?;

    // List all objects in the LFS store.
    let store_objects = list_lfs_objects(store).await?;

    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(Duration::from_secs(u64::from(retain_days) * 86400));

    let mut unreferenced = Vec::new();
    let mut retained = Vec::new();

    for obj in &store_objects {
        if referenced.contains(&obj.oid_hex) {
            continue;
        }
        if obj.created_at >= cutoff.as_secs() {
            retained.push(obj.clone());
        } else {
            unreferenced.push(obj.clone());
        }
    }

    if delete && !dry_run && !unreferenced.is_empty() {
        for obj in &unreferenced {
            let mut oid = [0u8; 32];
            hex_decode(obj.oid_hex.as_bytes(), &mut oid)?;
            let _ = store.delete(&oid).await;
        }
    }

    let total_bytes: u64 = unreferenced.iter().map(|o| o.size).sum();
    Ok(PruneReport {
        total_objects: store_objects.len() as u64,
        unreferenced_count: unreferenced.len() as u64,
        retained_count: retained.len() as u64,
        total_bytes_freed: total_bytes,
        objects: unreferenced,
        retained,
    })
}

/// Run fsck: verify integrity of all LFS objects in the store.
pub async fn run_fsck(store: &LfsObjectStore, repair: bool) -> Result<FsckReport> {
    let objects = list_lfs_objects(store).await?;
    let mut corrupt = Vec::new();
    let mut repaired = Vec::new();
    let total = objects.len() as u64;

    for obj in &objects {
        let mut oid = [0u8; 32];
        if hex_decode(obj.oid_hex.as_bytes(), &mut oid).is_err() {
            corrupt.push(FsckIssue {
                oid: obj.oid_hex.clone(),
                reason: "invalid OID in key path".into(),
            });
            continue;
        }

        match store.verify(&oid).await {
            Ok(_) => {} // Object is valid.
            Err(LfsError::ObjectCorrupt { .. }) => {
                if repair {
                    // Attempt repair from local cache.
                    if let Ok(bytes) = try_local_repair(&obj.oid_hex)
                        && store.put(&oid, bytes).await.is_ok()
                        && store.verify(&oid).await.is_ok()
                    {
                        repaired.push(obj.oid_hex.clone());
                        continue;
                    }
                }
                corrupt.push(FsckIssue {
                    oid: obj.oid_hex.clone(),
                    reason: "SHA-256 mismatch".into(),
                });
            }
            Err(LfsError::ObjectMissing { .. }) => {
                corrupt.push(FsckIssue {
                    oid: obj.oid_hex.clone(),
                    reason: "object disappeared during scan".into(),
                });
            }
            Err(e) => {
                corrupt.push(FsckIssue {
                    oid: obj.oid_hex.clone(),
                    reason: format!("{e}"),
                });
            }
        }
    }

    Ok(FsckReport {
        total,
        corrupt_count: corrupt.len() as u64,
        repaired_count: repaired.len() as u64,
        corrupt,
    })
}

/// Generate a cloud lifecycle policy for the LFS object prefix.
pub fn generate_lifecycle_policy(backend: &str, prefix: &str, expire_days: u32) -> String {
    match backend {
        "s3" | "aws" => s3_lifecycle_policy(prefix, expire_days),
        "gcs" | "gcp" | "google" => gcs_lifecycle_policy(prefix, expire_days),
        "azure" | "az" => azure_lifecycle_policy(prefix, expire_days),
        _ => format!("# Unsupported backend: {backend}. Supported: s3, gcs, azure"),
    }
}

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PruneReport {
    pub total_objects: u64,
    pub unreferenced_count: u64,
    pub retained_count: u64,
    pub total_bytes_freed: u64,
    pub objects: Vec<StoreObject>,
    pub retained: Vec<StoreObject>,
}

#[derive(Debug, Clone)]
pub struct FsckReport {
    pub total: u64,
    pub corrupt_count: u64,
    pub repaired_count: u64,
    pub corrupt: Vec<FsckIssue>,
}

#[derive(Debug, Clone)]
pub struct FsckIssue {
    pub oid: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct StoreObject {
    pub oid_hex: String,
    pub size: u64,
    pub created_at: u64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Collect all LFS OIDs referenced by reachable commits.
fn collect_referenced_oids(since: Option<&str>) -> Result<std::collections::HashSet<String>> {
    let mut referenced = std::collections::HashSet::new();

    // Walk all reachable commits and extract LFS pointer OIDs.
    let mut cmd = std::process::Command::new("git");
    cmd.args(["rev-list", "--objects", "--all"]);
    if let Some(s) = since {
        cmd.args(["--since", s]);
    }

    let output = cmd.output().map_err(CrabError::Io)?;
    if !output.status.success() {
        return Ok(referenced);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Each line is: "<oid> <path>" or just "<oid>"
        let Some(blob_oid) = line.split_whitespace().next() else {
            continue;
        };

        // Check if this blob is an LFS pointer (small enough).
        let type_output = std::process::Command::new("git")
            .args(["cat-file", "-t", blob_oid])
            .output();

        let obj_type = match type_output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => continue,
        };

        if obj_type != "blob" {
            continue;
        }

        // Read the blob content and check for LFS pointer.
        let blob_output = std::process::Command::new("git")
            .args(["cat-file", "blob", blob_oid])
            .output();

        let content = match blob_output {
            Ok(o) if o.status.success() => o.stdout,
            _ => continue,
        };

        // LFS pointers start with "version https://git-lfs.github.com/spec/v1"
        if content.starts_with(b"version https://git-lfs.github.com/spec/v1") {
            // Parse the OID from the pointer line: "oid sha256:<hex>"
            if let Some(oid_line) = content
                .split(|&b| b == b'\n')
                .find(|l| l.starts_with(b"oid sha256:"))
            {
                let oid_hex = String::from_utf8_lossy(&oid_line[10..74.min(oid_line.len())]);
                referenced.insert(oid_hex.to_string());
            }
        }
    }

    Ok(referenced)
}

/// List all LFS objects in the store.
async fn list_lfs_objects(store: &LfsObjectStore) -> Result<Vec<StoreObject>> {
    use futures_util::StreamExt;
    use object_store::path::Path;

    let prefix = Path::from(format!("{}/lfs/objects/", store.prefix()));
    let inner = store.store().inner();
    let mut stream = inner.list(Some(&prefix));

    let mut objects = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(meta) => {
                let size = meta.size;
                // Extract the OID hex from the path (last component).
                let path_str = meta.location.as_ref();
                let oid_hex = path_str.rsplit('/').next().unwrap_or(path_str).to_string();
                objects.push(StoreObject {
                    oid_hex,
                    size,
                    created_at: meta.last_modified.timestamp() as u64,
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "lifecycle: error listing LFS objects");
            }
        }
    }

    Ok(objects)
}

fn hex_decode(hex: &[u8], out: &mut [u8; 32]) -> Result<()> {
    if hex.len() != 64 {
        return Err(CrabError::Internal("invalid hex length".into()));
    }
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_nibble(hex[i * 2]).ok_or_else(|| CrabError::Internal("bad hex".into()))?;
        let lo = hex_nibble(hex[i * 2 + 1]).ok_or_else(|| CrabError::Internal("bad hex".into()))?;
        *byte = (hi << 4) | lo;
    }
    Ok(())
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn try_local_repair(oid_hex: &str) -> Result<bytes::Bytes> {
    let local_path = std::path::PathBuf::from(".git/lfs/objects")
        .join(&oid_hex[..2])
        .join(&oid_hex[2..4])
        .join(oid_hex);
    let data = std::fs::read(&local_path).map_err(CrabError::Io)?;
    Ok(bytes::Bytes::from(data))
}

// ---------------------------------------------------------------------------
// Cloud policy generators
// ---------------------------------------------------------------------------

fn s3_lifecycle_policy(prefix: &str, expire_days: u32) -> String {
    format!(
        r#"{{
  "Rules": [
    {{
      "Id": "crab-lfs-expire-{expire_days}d",
      "Status": "Enabled",
      "Filter": {{
        "Prefix": "{prefix}/lfs/objects/"
      }},
      "Expiration": {{
        "Days": {expire_days}
      }}
    }}
  ]
}}"#
    )
}

fn gcs_lifecycle_policy(prefix: &str, expire_days: u32) -> String {
    format!(
        r#"{{
  "lifecycle": {{
    "rule": [
      {{
        "action": {{
          "type": "Delete"
        }},
        "condition": {{
          "age": {expire_days},
          "matchesPrefix": ["{prefix}/lfs/objects/"]
        }}
      }}
    ]
  }}
}}"#
    )
}

fn azure_lifecycle_policy(prefix: &str, expire_days: u32) -> String {
    format!(
        r#"{{
  "rules": [
    {{
      "name": "crab-lfs-expire-{expire_days}d",
      "enabled": true,
      "type": "Lifecycle",
      "definition": {{
        "filters": {{
          "blobTypes": ["blockBlob"],
          "prefixMatch": ["{prefix}/lfs/objects/"]
        }},
        "actions": {{
          "baseBlob": {{
            "delete": {{
              "daysAfterModificationGreaterThan": {expire_days}
            }}
          }}
        }}
      }}
    }}
  ]
}}"#
    )
}
