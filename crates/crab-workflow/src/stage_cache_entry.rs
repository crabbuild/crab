//! Durable workflow stage cache entry contracts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hasher::{TreeEntry, TreeEntryKind, hash_tree_entries};
use crate::{OutKind, Result, StageName, WorkflowError};
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

/// Iterate cached artifacts in stable output/metric/plot order.
///
/// A structured metric or plot may also appear in `outs` to carry its cache
/// policy. Identical records are emitted once; validation rejects conflicts.
pub fn cached_artifacts(entry: &StageCacheEntry) -> impl Iterator<Item = &CachedOut> {
    let mut paths = BTreeSet::new();
    entry
        .outs
        .iter()
        .chain(entry.metrics.iter())
        .chain(entry.plots.iter())
        .filter(move |output| paths.insert(output.path.clone()))
}

/// Validate a cache entry before it is read, materialized, or published.
///
/// Cache entries can arrive from an untrusted remote. The validator keeps
/// their paths inside the worktree, enforces canonical content identities,
/// and checks directory metadata before any filesystem operation occurs.
pub fn validate_stage_cache_entry(entry: &StageCacheEntry) -> Result<()> {
    validate_stage_cache_entry_at(entry, None)
}

/// Validate a cache entry while allowing legacy absolute paths under a
/// caller-provided repository root. Remote manifests must use the strict
/// [`validate_stage_cache_entry`] form because they have no trusted root.
pub(crate) fn validate_stage_cache_entry_at(
    entry: &StageCacheEntry,
    repository_root: Option<&Path>,
) -> Result<()> {
    let stage_hash = entry.stage_hash.as_hex();
    if entry.schema_version == 0 || entry.schema_version > ENTRY_SCHEMA_MAX_SUPPORTED {
        return Err(cache_entry_invalid(
            &stage_hash,
            format!("unsupported schema version {}", entry.schema_version),
        ));
    }
    StageName::parse_effective(&entry.stage_name).map_err(|error| {
        cache_entry_invalid(&stage_hash, format!("invalid stage name: {error}"))
    })?;
    validate_command(&entry.cmd, &stage_hash)?;
    if entry.attempts == 0 {
        return Err(cache_entry_invalid(
            &stage_hash,
            "attempt count must be at least one".to_owned(),
        ));
    }

    let mut paths = BTreeMap::new();
    for (kind, outputs) in [
        ("out", entry.outs.as_slice()),
        ("metric", entry.metrics.as_slice()),
        ("plot", entry.plots.as_slice()),
    ] {
        for (index, output) in outputs.iter().enumerate() {
            validate_cached_out(
                output,
                &stage_hash,
                kind,
                index,
                repository_root,
                &mut paths,
            )?;
        }
    }
    Ok(())
}

/// Validate a directory manifest before it is joined to a staging path.
pub(crate) fn validate_tree_manifest(manifest: &[TreeManifestEntry]) -> Result<([u8; 32], u64)> {
    let mut paths = BTreeSet::new();
    let mut kinds = BTreeMap::new();
    let mut entries = Vec::with_capacity(manifest.len());
    let mut total_size = 0_u64;

    for entry in manifest {
        let path = validate_relative_path(&entry.path, "directory manifest path")?;
        if !paths.insert(path.clone()) {
            return Err(cache_entry_invalid(
                "",
                format!(
                    "directory manifest contains duplicate path {:?}",
                    entry.path
                ),
            ));
        }
        if entry.mode > 0o7777 {
            return Err(cache_entry_invalid(
                "",
                format!("directory manifest mode is invalid for {:?}", entry.path),
            ));
        }

        let (kind, file_hash) = match entry.kind.as_str() {
            "file" => {
                let hash = decode_b3_hash(&entry.hash).ok_or_else(|| {
                    cache_entry_invalid(
                        "",
                        format!("directory file hash is malformed for {:?}", entry.path),
                    )
                })?;
                (TreeEntryKind::File, hash)
            }
            "dir" => {
                if !entry.hash.is_empty() || entry.size != 0 {
                    return Err(cache_entry_invalid(
                        "",
                        format!("directory metadata is invalid for {:?}", entry.path),
                    ));
                }
                (TreeEntryKind::Directory, [0; 32])
            }
            _ => {
                return Err(cache_entry_invalid(
                    "",
                    format!("directory manifest kind is invalid for {:?}", entry.path),
                ));
            }
        };

        if kinds.insert(path.clone(), kind).is_some() {
            return Err(cache_entry_invalid(
                "",
                format!(
                    "directory manifest contains duplicate path {:?}",
                    entry.path
                ),
            ));
        }
        total_size = total_size.checked_add(entry.size).ok_or_else(|| {
            cache_entry_invalid("", "directory manifest size overflows u64".to_owned())
        })?;
        entries.push(TreeEntry {
            path,
            kind,
            file_hash,
            size: entry.size,
            mode: entry.mode,
        });
    }

    for path in paths {
        let mut ancestor = path.parent();
        while let Some(candidate) = ancestor.filter(|path| !path.as_os_str().is_empty()) {
            if kinds
                .get(candidate)
                .is_some_and(|kind| *kind == TreeEntryKind::File)
            {
                return Err(cache_entry_invalid(
                    "",
                    format!("directory manifest nests a path below file {:?}", candidate),
                ));
            }
            ancestor = candidate.parent();
        }
    }

    Ok((hash_tree_entries(&entries), total_size))
}

/// Decode the canonical content hash used by cached files and tree members.
pub(crate) fn decode_b3_hash(value: &str) -> Option<[u8; 32]> {
    let bytes = value.as_bytes();
    if bytes.len() != 67 || &bytes[..3] != b"b3:" {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in bytes[3..].chunks_exact(2).enumerate() {
        if !pair.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        digest[index] = (high << 4) | low;
    }
    Some(digest)
}

fn validate_command(command: &CachedCmd, stage_hash: &str) -> Result<()> {
    let invalid = |detail: String| cache_entry_invalid(stage_hash, detail);
    match command {
        CachedCmd::Argv { argv } if argv.is_empty() || argv[0].is_empty() => Err(invalid(
            "argv command must contain a non-empty executable".to_owned(),
        )),
        CachedCmd::Shell { shell } if shell.trim().is_empty() => {
            Err(invalid("shell command must not be empty".to_owned()))
        }
        CachedCmd::ShellList { commands }
            if commands.is_empty() || commands.iter().any(|command| command.trim().is_empty()) =>
        {
            Err(invalid("shell-list commands must not be empty".to_owned()))
        }
        _ => Ok(()),
    }
}

fn validate_cached_out(
    output: &CachedOut,
    stage_hash: &str,
    category: &str,
    index: usize,
    repository_root: Option<&Path>,
    paths: &mut BTreeMap<PathBuf, (String, CachedOut)>,
) -> Result<()> {
    let path = validate_cached_path(
        &output.path,
        &format!("{category}[{index}] path"),
        repository_root,
    )?;
    if let Some((existing_category, existing)) = paths.get(&path) {
        if existing != output {
            return Err(cache_entry_invalid(
                stage_hash,
                format!(
                    "cached artifact path {:?} has conflicting {existing_category} and {category} records",
                    output.path
                ),
            ));
        }
        return Ok(());
    }
    paths.insert(path, (category.to_owned(), output.clone()));
    if output
        .remote
        .as_deref()
        .is_some_and(|remote| remote.is_empty() || remote.chars().any(char::is_control))
    {
        return Err(cache_entry_invalid(
            stage_hash,
            format!("{category}[{index}] remote name is invalid"),
        ));
    }
    if output.mode > 0o7777 {
        return Err(cache_entry_invalid(
            stage_hash,
            format!("{category}[{index}] mode is invalid"),
        ));
    }
    if decode_b3_hash(&output.file_hash).is_none() {
        return Err(cache_entry_invalid(
            stage_hash,
            format!("{category}[{index}] content hash is malformed"),
        ));
    }

    match output.kind {
        OutKind::File | OutKind::Stdout => {
            if output.tree_manifest.is_some() {
                return Err(cache_entry_invalid(
                    stage_hash,
                    format!("{category}[{index}] file output has a tree manifest"),
                ));
            }
        }
        OutKind::Directory => {
            let Some(manifest) = output.tree_manifest.as_deref() else {
                return Err(cache_entry_invalid(
                    stage_hash,
                    format!("{category}[{index}] directory output has no tree manifest"),
                ));
            };
            let (tree_hash, total_size) = validate_tree_manifest(manifest).map_err(|error| {
                cache_entry_invalid(stage_hash, format!("{category}[{index}] {error}"))
            })?;
            let expected_hash = format!(
                "b3:{}",
                tree_hash
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            if output.file_hash != expected_hash || output.size != total_size {
                return Err(cache_entry_invalid(
                    stage_hash,
                    format!("{category}[{index}] tree hash or size does not match manifest"),
                ));
            }
        }
    }
    Ok(())
}

fn validate_relative_path(value: &str, label: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if value.is_empty() || value == "." || value.chars().any(char::is_control) {
        return Err(cache_entry_invalid("", format!("{label} is invalid")));
    }
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(cache_entry_invalid(
            "",
            format!("{label} must be relative and must not contain '..'"),
        ));
    }
    Ok(path.to_path_buf())
}

fn validate_cached_path(
    path: &Path,
    label: &str,
    repository_root: Option<&Path>,
) -> Result<PathBuf> {
    let has_parent_component = path
        .components()
        .any(|component| matches!(component, Component::ParentDir));
    if path.is_absolute() {
        let Some(root) = repository_root else {
            return Err(cache_entry_invalid(
                "",
                format!("{label} must be repository-relative"),
            ));
        };
        if has_parent_component
            || path == root
            || !path.starts_with(root)
            || path.as_os_str().is_empty()
            || path.to_string_lossy().chars().any(char::is_control)
        {
            return Err(cache_entry_invalid(
                "",
                format!("{label} must stay inside the repository root"),
            ));
        }
        return Ok(path.to_path_buf());
    }
    validate_relative_path(&path.to_string_lossy(), label)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn cache_entry_invalid(stage_hash: &str, detail: String) -> WorkflowError {
    WorkflowError::CacheEntryInvalid {
        stage_hash: stage_hash.to_owned(),
        detail,
    }
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
    fn cache_entry_validation_accepts_identical_policy_and_metric_records() {
        let entry = entry_with_outs(vec![cached_out("metrics.json", true)]);
        validate_stage_cache_entry(&entry).unwrap();

        let paths: Vec<_> = cached_artifacts(&entry)
            .map(|out| out.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, ["metrics.json", "plots/curve.json"]);
    }

    #[test]
    fn cache_entry_validation_rejects_conflicting_duplicate_records() {
        let entry = entry_with_outs(vec![cached_out("metrics.json", false)]);
        let error = validate_stage_cache_entry(&entry).unwrap_err();
        assert!(matches!(error, WorkflowError::CacheEntryInvalid { .. }));
        assert!(error.to_string().contains("conflicting"));
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

    #[test]
    fn cache_entry_validation_accepts_executor_shape() {
        validate_stage_cache_entry(&entry_with_outs(vec![cached_out("model.bin", true)])).unwrap();
    }

    #[test]
    fn cache_entry_validation_rejects_unsafe_output_path() {
        let mut entry = entry_with_outs(vec![cached_out("../escape", true)]);
        let error = validate_stage_cache_entry(&entry).unwrap_err();
        assert!(matches!(error, WorkflowError::CacheEntryInvalid { .. }));

        entry.outs[0].path = PathBuf::from("model.bin");
        entry.outs[0].file_hash = "b3:short".to_owned();
        let error = validate_stage_cache_entry(&entry).unwrap_err();
        assert!(matches!(error, WorkflowError::CacheEntryInvalid { .. }));
    }

    #[test]
    fn tree_manifest_validation_rejects_duplicate_and_nested_file_paths() {
        let duplicate = vec![
            TreeManifestEntry {
                path: "weights.bin".to_owned(),
                kind: "file".to_owned(),
                hash: format!("b3:{}", "ab".repeat(32)),
                size: 1,
                mode: 0o644,
            },
            TreeManifestEntry {
                path: "weights.bin".to_owned(),
                kind: "file".to_owned(),
                hash: format!("b3:{}", "cd".repeat(32)),
                size: 1,
                mode: 0o644,
            },
        ];
        assert!(validate_tree_manifest(&duplicate).is_err());

        let nested_file = vec![
            TreeManifestEntry {
                path: "weights".to_owned(),
                kind: "file".to_owned(),
                hash: format!("b3:{}", "ab".repeat(32)),
                size: 1,
                mode: 0o644,
            },
            TreeManifestEntry {
                path: "weights/member.bin".to_owned(),
                kind: "file".to_owned(),
                hash: format!("b3:{}", "cd".repeat(32)),
                size: 1,
                mode: 0o644,
            },
        ];
        assert!(validate_tree_manifest(&nested_file).is_err());
    }
}
