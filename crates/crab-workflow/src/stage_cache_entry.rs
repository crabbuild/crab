//! Durable workflow stage cache entry contracts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::OutKind;
use crab_types::workflow::StageHash;

/// Current on-disk entry schema.
///
/// Newer readers migrate older entries up; older readers refuse newer entries.
pub const ENTRY_SCHEMA_VERSION: u16 = 3;

/// Maximum schema version this crate can read.
pub const ENTRY_SCHEMA_MAX_SUPPORTED: u16 = 3;

/// One output recorded in a stage cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedOut {
    pub path: PathBuf,
    pub kind: OutKind,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub push: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Hex-encoded Blake3 of the output file contents.
    pub file_hash: String,
    pub size: u64,
    /// Unix mode bits.
    pub mode: u32,
    /// Tree manifest entries for directory outs. `None` for file outs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_manifest: Option<Vec<TreeManifestEntry>>,
}

/// One entry in a cached directory manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeManifestEntry {
    /// Relative path within the directory.
    pub path: String,
    /// `"file"` or `"dir"`.
    pub kind: String,
    /// `b3:<hex>` hash of the file contents. Zero-hash for directories.
    pub hash: String,
    /// File size in bytes. 0 for directories.
    pub size: u64,
    /// Unix mode bits.
    pub mode: u32,
}

/// Full cache entry for a single stage execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageCacheEntry {
    pub schema_version: u16,
    pub stage_hash: StageHash,
    pub stage_name: String,
    /// Canonical command record: shell string, argv vector, or shell-list.
    pub cmd: CachedCmd,
    pub outs: Vec<CachedOut>,
    pub metrics: Vec<CachedOut>,
    pub plots: Vec<CachedOut>,
    /// RFC3339 with millisecond precision.
    pub executed_at: String,
    /// Monotonic clock duration.
    pub duration_ms: u64,
    /// `Some` only for nondeterministic stages.
    pub exec_id: Option<String>,
    pub attempts: u32,
    pub host_fingerprint: String,
}

impl StageCacheEntry {
    /// Return whether this cache entry may be published remotely.
    #[must_use]
    pub fn remote_push_enabled(&self) -> bool {
        self.outs.iter().all(|out| out.push)
    }
}

/// Iterate every cached artifact record in stable output/metric/plot order.
pub fn cached_artifacts(entry: &StageCacheEntry) -> impl Iterator<Item = &CachedOut> {
    entry
        .outs
        .iter()
        .chain(entry.metrics.iter())
        .chain(entry.plots.iter())
}

/// Canonical command record.
///
/// The variant preserves whether the user declared argv, a shell string, or a
/// shell-list so cache hits replay exactly the declared command form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CachedCmd {
    Argv { argv: Vec<String> },
    Shell { shell: String },
    ShellList { commands: Vec<String> },
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_out(path: &str, push: bool) -> CachedOut {
        CachedOut {
            path: PathBuf::from(path),
            kind: OutKind::File,
            push,
            remote: None,
            file_hash: format!("b3:{}", "ab".repeat(32)),
            size: 1,
            mode: 0o644,
            tree_manifest: None,
        }
    }

    fn entry_with_outs(outs: Vec<CachedOut>) -> StageCacheEntry {
        StageCacheEntry {
            schema_version: ENTRY_SCHEMA_VERSION,
            stage_hash: StageHash([0xab; 32]),
            stage_name: "train".to_owned(),
            cmd: CachedCmd::Shell {
                shell: "python train.py".to_owned(),
            },
            outs,
            metrics: vec![cached_out("metrics.json", true)],
            plots: vec![cached_out("plots/curve.json", true)],
            executed_at: "2026-01-01T00:00:00.000Z".to_owned(),
            duration_ms: 12,
            exec_id: None,
            attempts: 1,
            host_fingerprint: "host".to_owned(),
        }
    }

    #[test]
    fn remote_push_enabled_requires_all_outs_pushable() {
        assert!(entry_with_outs(vec![cached_out("model.bin", true)]).remote_push_enabled());
        assert!(!entry_with_outs(vec![cached_out("model.bin", false)]).remote_push_enabled());
    }

    #[test]
    fn cached_artifacts_preserves_output_metric_plot_order() {
        let entry = entry_with_outs(vec![cached_out("model.bin", true)]);
        let paths: Vec<_> = cached_artifacts(&entry)
            .map(|out| out.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, ["model.bin", "metrics.json", "plots/curve.json"]);
    }

    #[test]
    fn cached_out_serde_defaults_push_to_true() {
        let json = format!(
            r#"{{
                "path":"model.bin",
                "kind":"File",
                "file_hash":"b3:{}",
                "size":1,
                "mode":420
            }}"#,
            "ab".repeat(32)
        );
        let out: CachedOut = serde_json::from_str(&json).unwrap();
        assert!(out.push);
        assert!(out.tree_manifest.is_none());
    }
}
