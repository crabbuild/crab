//! Stage cache entries and local cache I/O.
//!
//! A `StageCacheEntry` is the durable record of a successful stage
//! execution: the stage hash, the set of output hashes it produced,
//! plus the provenance bits (timestamp, duration, host fingerprint,
//! attempts, `exec_id` for nondeterministic stages). Entries are
//! serialized as canonical JSON and stored locally under the
//! `stages/` subtree of the chunk cache.
//!
//! Entry schemas are versioned (`schema_version`). Newer schemas are
//! refused with `CacheEntrySchemaNewer`; older schemas are migrated
//! up to the current version before use and re-serialized on next
//! write. A v1→v1 identity migration scaffolds the ladder.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::stage::OutKind;
use crate::stage_cache_entry::{
    MAX_STAGE_CACHE_ARTIFACT_BYTES, MAX_STAGE_CACHE_ENTRY_BYTES, decode_b3_hash,
    validate_stage_cache_entry, validate_stage_cache_entry_at,
};
pub use crate::{
    CachedCmd, CachedOut, ENTRY_SCHEMA_MAX_SUPPORTED, ENTRY_SCHEMA_VERSION, StageCacheEntry,
    TreeManifestEntry, cached_artifacts,
};
use crate::{Result, WorkflowError as CrabError};
use crab_types::workflow::StageHash;

/// Default minimum free disk space (100 MB) before skipping cache writes.
pub const DEFAULT_MIN_CACHE_HEADROOM_BYTES: u64 = 100 * 1024 * 1024;

/// Process-wide flag indicating the local cache directory is not
/// writable. Once set, all cache reads return `None` and writes are
/// skipped. This avoids repeated I/O errors on every stage when the
/// cache lives on a read-only filesystem.
static CACHE_DISABLED: AtomicBool = AtomicBool::new(false);

/// Returns `true` if the cache has been disabled due to a read-only
/// filesystem or permission error.
pub fn is_cache_disabled() -> bool {
    CACHE_DISABLED.load(Ordering::Relaxed)
}

/// Probe the cache directory for writability. If the directory is
/// read-only (EACCES or EROFS), sets the global `CACHE_DISABLED`
/// flag and emits a warning. Call this once at startup before any
/// stage execution.
pub fn probe_cache_writable(cache_root: &Path) {
    // Ensure the directory exists (or try to create it).
    if let Err(e) = std::fs::create_dir_all(cache_root) {
        if is_permission_or_readonly(&e) {
            tracing::warn!(
                cache_root = %cache_root.display(),
                "cache directory not writable; operating without cache"
            );
            CACHE_DISABLED.store(true, Ordering::Relaxed);
            return;
        }
        // Other errors (e.g. path component is a file) — don't disable,
        // let the first real write surface the error.
        return;
    }

    // Try writing a probe file to confirm actual writability.
    let probe = cache_root.join(".crab_cache_probe");
    match std::fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
        }
        Err(e) if is_permission_or_readonly(&e) => {
            tracing::warn!(
                cache_root = %cache_root.display(),
                "cache directory not writable; operating without cache"
            );
            CACHE_DISABLED.store(true, Ordering::Relaxed);
        }
        Err(_) => {
            // Non-permission error — let the first real write surface it.
        }
    }
}

/// Reset the cache-disabled flag. Primarily for tests that need to
/// exercise the probe logic multiple times in the same process.
pub fn reset_cache_disabled() {
    CACHE_DISABLED.store(false, Ordering::Relaxed);
}

/// Check whether an I/O error indicates a read-only or permission-denied
/// condition (EACCES or EROFS).
fn is_permission_or_readonly(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
    ) || {
        #[cfg(unix)]
        {
            err.raw_os_error()
                .map(|code| code == libc::EACCES || code == libc::EROFS)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

/// Check available disk space at the given path. Returns `None` if
/// the check is not supported or fails.
#[cfg(unix)]
pub fn available_disk_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs is a standard POSIX call; zeroed struct and a
    // nul-terminated path satisfy its contract.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn available_disk_space(_path: &Path) -> Option<u64> {
    None
}

/// Load an entry from the local cache root for the given stage hash.
///
/// Returns `Ok(None)` when the entry is missing or when the cache has
/// been disabled (read-only filesystem). Returns
/// `CacheEntrySchemaNewer` when an on-disk entry is stamped with a
/// higher schema version than this binary supports. Older schemas
/// are migrated up to the current version.
pub fn read_local(cache_root: &Path, hash: &StageHash) -> Result<Option<StageCacheEntry>> {
    if is_cache_disabled() {
        return Ok(None);
    }
    let path = entry_path(cache_root, hash);
    let bytes = match read_stage_cache_file_bounded(&path, &hash.as_hex()) {
        Ok(bytes) => bytes,
        Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };

    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| CrabError::WorkflowJournalCorrupt {
            run_id: hash.as_hex(),
            detail: "stage cache entry is not valid JSON".to_owned(),
        })?;

    let found = raw
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| CrabError::WorkflowJournalCorrupt {
            run_id: hash.as_hex(),
            detail: "stage cache entry missing schema_version".to_owned(),
        })?;

    if found > ENTRY_SCHEMA_MAX_SUPPORTED {
        return Err(CrabError::CacheEntrySchemaNewer {
            stage_hash: hash.as_hex(),
            found,
            supported: ENTRY_SCHEMA_MAX_SUPPORTED,
        });
    }

    let migrated = migrate(found, raw)?;
    let entry: StageCacheEntry =
        serde_json::from_value(migrated).map_err(|e| CrabError::WorkflowJournalCorrupt {
            run_id: hash.as_hex(),
            detail: format!("stage cache entry shape mismatch: {e}"),
        })?;
    if entry.stage_hash != *hash {
        return Err(CrabError::CacheEntryHashMismatch {
            manifest_hash: entry.stage_hash.as_hex(),
            local_hash: hash.as_hex(),
        });
    }
    validate_stage_cache_entry_at(&entry, cache_validation_root(cache_root))?;
    Ok(Some(entry))
}

/// Persist an entry to the local cache root. Overwrites any existing
/// entry at the same stage hash (stage hashes are deterministic —
/// collisions are writes of the same logical value).
///
/// Returns `Ok(())` immediately (no-op) when the cache has been
/// disabled due to a read-only filesystem.
pub fn write_local(cache_root: &Path, entry: &StageCacheEntry) -> Result<()> {
    if is_cache_disabled() {
        return Ok(());
    }
    validate_stage_cache_entry_at(entry, cache_validation_root(cache_root))?;
    let path = entry_path(cache_root, &entry.stage_hash);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            if is_permission_or_readonly(&e) {
                tracing::warn!(
                    cache_root = %cache_root.display(),
                    "cache directory not writable; operating without cache"
                );
                CACHE_DISABLED.store(true, Ordering::Relaxed);
                return CrabError::Io(e);
            }
            CrabError::Io(e)
        })?;
    }
    let bytes = canonical_json(entry)?;
    if bytes.len() > MAX_STAGE_CACHE_ENTRY_BYTES {
        return Err(CrabError::CacheEntryInvalid {
            stage_hash: entry.stage_hash.as_hex(),
            detail: format!(
                "stage cache manifest is {} bytes; safety limit is {MAX_STAGE_CACHE_ENTRY_BYTES}",
                bytes.len()
            ),
        });
    }
    match atomic_write(&path, &bytes) {
        Ok(()) => Ok(()),
        Err(CrabError::Io(e)) if is_permission_or_readonly(&e) => {
            tracing::warn!(
                cache_root = %cache_root.display(),
                "cache directory not writable; operating without cache"
            );
            CACHE_DISABLED.store(true, Ordering::Relaxed);
            Ok(())
        }
        Err(other) => Err(other),
    }
}

/// Compute the on-disk path for a stage cache entry. Mirrors the
/// remote `workflow/stages/<2-char-shard>/<hex>.json` layout so
/// `--cache-push` can translate a local entry directly into a
/// remote `PUT`.
pub fn entry_path(cache_root: &Path, hash: &StageHash) -> PathBuf {
    let hex = hash.as_hex();
    cache_root
        .join("stages")
        .join(&hex[..2])
        .join(format!("{hex}.json"))
}

/// Store cached artifact bytes in the local content cache for later cache-hit replay.
pub fn store_local_xorbs<'a>(
    cache_root: &Path,
    outs: impl IntoIterator<Item = &'a CachedOut>,
    working_dir: Option<&Path>,
) -> Result<()> {
    if is_cache_disabled() {
        return Ok(());
    }

    let base = working_dir.unwrap_or_else(|| Path::new("."));
    for out in outs {
        match out.kind {
            OutKind::File | OutKind::Stdout => {
                let source = if out.path.is_absolute() {
                    out.path.clone()
                } else {
                    base.join(&out.path)
                };
                let bytes = read_artifact_file_bounded(&source, &out.file_hash)?;
                write_local_xorb(cache_root, &out.file_hash, &bytes)?;
            }
            OutKind::Directory => {
                let Some(manifest) = out.tree_manifest.as_ref() else {
                    continue;
                };
                let root = if out.path.is_absolute() {
                    out.path.clone()
                } else {
                    base.join(&out.path)
                };
                for entry in manifest {
                    if entry.kind == "dir" {
                        continue;
                    }
                    let path = root.join(&entry.path);
                    let bytes = read_artifact_file_bounded(&path, &entry.hash)?;
                    write_local_xorb(cache_root, &entry.hash, &bytes)?;
                }
            }
        }
    }
    Ok(())
}

/// Read bytes from the local content cache, verifying the address.
pub fn read_local_xorb(cache_root: &Path, xorb_hash: &str) -> Result<Option<Vec<u8>>> {
    if is_cache_disabled() {
        return Ok(None);
    }

    let path = local_xorb_path(cache_root, xorb_hash)?;
    let bytes = match read_artifact_file_bounded(&path, xorb_hash) {
        Ok(bytes) => bytes,
        Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    Ok(Some(bytes))
}

fn write_local_xorb(cache_root: &Path, xorb_hash: &str, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 > MAX_STAGE_CACHE_ARTIFACT_BYTES {
        return Err(CrabError::CacheEntryCorrupt {
            stage_hash: String::new(),
            path: xorb_hash.to_owned(),
            expected: format!("at most {MAX_STAGE_CACHE_ARTIFACT_BYTES} bytes"),
            actual: format!("{} bytes", bytes.len()),
        });
    }
    let path = local_xorb_path(cache_root, xorb_hash)?;
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        if is_permission_or_readonly(&e) {
            tracing::warn!(
                cache_root = %cache_root.display(),
                "cache directory not writable; operating without cache"
            );
            CACHE_DISABLED.store(true, Ordering::Relaxed);
            return Ok(());
        }
        return Err(CrabError::Io(e));
    }
    match atomic_write(&path, bytes) {
        Ok(()) => Ok(()),
        Err(CrabError::Io(e)) if is_permission_or_readonly(&e) => {
            tracing::warn!(
                cache_root = %cache_root.display(),
                "cache directory not writable; operating without cache"
            );
            CACHE_DISABLED.store(true, Ordering::Relaxed);
            Ok(())
        }
        Err(other) => Err(other),
    }
}

fn read_artifact_file_bounded(path: &Path, expected_hash: &str) -> Result<Vec<u8>> {
    let metadata = std::fs::metadata(path).map_err(CrabError::Io)?;
    if metadata.len() > MAX_STAGE_CACHE_ARTIFACT_BYTES {
        return Err(CrabError::CacheEntryCorrupt {
            stage_hash: String::new(),
            path: path.display().to_string(),
            expected: format!("{expected_hash} and at most {MAX_STAGE_CACHE_ARTIFACT_BYTES} bytes"),
            actual: format!("{} bytes", metadata.len()),
        });
    }

    let file = std::fs::File::open(path).map_err(CrabError::Io)?;
    let mut bytes = Vec::new();
    file.take(MAX_STAGE_CACHE_ARTIFACT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(CrabError::Io)?;
    if bytes.len() as u64 > MAX_STAGE_CACHE_ARTIFACT_BYTES {
        return Err(CrabError::CacheEntryCorrupt {
            stage_hash: String::new(),
            path: path.display().to_string(),
            expected: format!("{expected_hash} and at most {MAX_STAGE_CACHE_ARTIFACT_BYTES} bytes"),
            actual: format!("{} bytes", bytes.len()),
        });
    }
    let actual_hash = format!("b3:{}", blake3::hash(&bytes).to_hex());
    if actual_hash != expected_hash {
        return Err(CrabError::CacheEntryCorrupt {
            stage_hash: String::new(),
            path: path.display().to_string(),
            expected: expected_hash.to_owned(),
            actual: actual_hash,
        });
    }
    Ok(bytes)
}

fn read_stage_cache_file_bounded(path: &Path, stage_hash: &str) -> Result<Vec<u8>> {
    let metadata = std::fs::metadata(path).map_err(CrabError::Io)?;
    if metadata.len() > MAX_STAGE_CACHE_ENTRY_BYTES as u64 {
        return Err(CrabError::CacheEntryInvalid {
            stage_hash: stage_hash.to_owned(),
            detail: format!(
                "stage cache manifest is {} bytes; safety limit is {MAX_STAGE_CACHE_ENTRY_BYTES}",
                metadata.len()
            ),
        });
    }

    let file = std::fs::File::open(path).map_err(CrabError::Io)?;
    let mut bytes = Vec::new();
    file.take(MAX_STAGE_CACHE_ENTRY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(CrabError::Io)?;
    if bytes.len() > MAX_STAGE_CACHE_ENTRY_BYTES {
        return Err(CrabError::CacheEntryInvalid {
            stage_hash: stage_hash.to_owned(),
            detail: format!(
                "stage cache manifest is larger than the safety limit of {MAX_STAGE_CACHE_ENTRY_BYTES} bytes"
            ),
        });
    }
    Ok(bytes)
}

fn local_xorb_path(cache_root: &Path, xorb_hash: &str) -> Result<PathBuf> {
    let Some(digest) = decode_b3_hash(xorb_hash) else {
        return Err(CrabError::Internal(format!(
            "invalid local xorb hash '{xorb_hash}'"
        )));
    };
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(cache_root
        .join("xorbs")
        .join(&hex[..2])
        .join(format!("{hex}.xorb")))
}

/// Serialize an entry to canonical JSON: sorted keys, no trailing
/// whitespace, UTF-8. `serde_json` sorts map keys when the value
/// comes from `to_value` → `Value::Object` (which uses a BTreeMap),
/// so round-trip through `Value` first.
fn canonical_json(entry: &StageCacheEntry) -> Result<Vec<u8>> {
    let value = serde_json::to_value(entry)
        .map_err(|e| CrabError::Internal(format!("stage cache entry serialization failed: {e}")))?;
    let canonical = canonicalize(value);
    serde_json::to_vec(&canonical).map_err(|e| {
        CrabError::Internal(format!(
            "stage cache entry canonical serialization failed: {e}"
        ))
    })
}

/// Recursively reorder object keys so serialization is deterministic.
/// Arrays preserve their declared order.
fn canonicalize(value: serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    match value {
        Value::Object(map) => {
            // BTreeMap gives sorted keys; collect into a fresh Map
            // (which preserves insertion order in serde_json) to keep
            // the sort stable across serializations.
            let mut sorted: std::collections::BTreeMap<String, Value> = map.into_iter().collect();
            let mut out = Map::with_capacity(sorted.len());
            while let Some((k, v)) = sorted.pop_first() {
                out.insert(k, canonicalize(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

/// Migration ladder. Each step reads the previous shape and produces
/// the next. Migrations are chained: a v1 entry passes through
/// `migrate_v1_to_v2` to reach the current schema.
fn migrate(from: u16, value: serde_json::Value) -> Result<serde_json::Value> {
    match from {
        1 => {
            let v2 = migrate_v1_to_v2(value)?;
            Ok(migrate_v2_to_v3(v2)?)
        }
        2 => migrate_v2_to_v3(value),
        3 => Ok(value),
        other => Err(CrabError::Internal(format!(
            "no migration path for stage cache entry schema v{other}"
        ))),
    }
}

fn migrate_v2_to_v3(mut value: serde_json::Value) -> Result<serde_json::Value> {
    let obj = value
        .as_object_mut()
        .ok_or_else(|| CrabError::Internal("stage cache entry is not a JSON object".to_owned()))?;
    obj.insert(
        "schema_version".to_owned(),
        serde_json::Value::Number(3.into()),
    );
    Ok(value)
}

/// Migrate a v1 cache entry to v2.
///
/// v1 (Phase 1) entries have file outs only, no remote refs, no tree
/// manifests. v2 adds:
/// - `tree_manifest: null` on each out (file outs don't have one).
/// - `source: "Local"` (Phase 1 entries are always local).
/// - `attempts: 1` (Phase 1 never retried — field may already exist
///   but we ensure it's present).
///
/// The migration is lossless: all v1 fields are preserved verbatim.
fn migrate_v1_to_v2(mut value: serde_json::Value) -> Result<serde_json::Value> {
    let obj = value
        .as_object_mut()
        .ok_or_else(|| CrabError::Internal("stage cache entry is not a JSON object".to_owned()))?;

    // Bump schema_version to 2.
    obj.insert(
        "schema_version".to_owned(),
        serde_json::Value::Number(2.into()),
    );

    // Ensure `attempts` is present (Phase 1 entries always ran once).
    obj.entry("attempts")
        .or_insert(serde_json::Value::Number(1.into()));

    // Ensure each out has `tree_manifest: null` (file outs don't have one).
    for outs_key in &["outs", "metrics", "plots"] {
        if let Some(outs_val) = obj.get_mut(*outs_key)
            && let Some(outs_arr) = outs_val.as_array_mut()
        {
            for out in outs_arr.iter_mut() {
                if let Some(out_obj) = out.as_object_mut() {
                    out_obj
                        .entry("tree_manifest".to_owned())
                        .or_insert(serde_json::Value::Null);
                }
            }
        }
    }

    Ok(value)
}

/// Atomic write: tempfile + rename. Same-filesystem by construction
/// because the temp lives under the cache root.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        CrabError::Internal(format!(
            "stage cache entry path has no parent: {}",
            path.display()
        ))
    })?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(CrabError::Io)?;
    use std::io::Write;
    tmp.write_all(bytes).map_err(CrabError::Io)?;
    tmp.as_file().sync_all().map_err(CrabError::Io)?;
    tmp.persist(path).map_err(|e| CrabError::Io(e.error))?;
    Ok(())
}

/// Pre-materialization check for cache-hit overwrites.
///
/// Returns `Ok(())` when the target file does not exist, has the
/// same hash as the cache entry (no-op write), or when `force` is
/// set. Returns `StageOverwriteConflict` when the target has a
/// different hash, no-overwrite was requested, or the target has
/// uncommitted git changes.
pub fn overwrite_policy(
    stage_name: &str,
    target: &Path,
    incoming: &CachedOut,
    current_on_disk: Option<&CurrentFile>,
    flags: OverwriteFlags,
) -> Result<OverwriteDecision> {
    let Some(current) = current_on_disk else {
        return Ok(OverwriteDecision::Write);
    };

    if current.file_hash == incoming.file_hash && current.mode == incoming.mode {
        return Ok(OverwriteDecision::NoOp);
    }

    if flags.no_overwrite {
        return Err(CrabError::StageOverwriteConflict {
            stage: stage_name.to_owned(),
            path: target.to_path_buf(),
            reason: "cache hit would overwrite a different file and --no-overwrite is set",
        });
    }

    if current.git_dirty && !flags.force {
        return Err(CrabError::StageOverwriteConflict {
            stage: stage_name.to_owned(),
            path: target.to_path_buf(),
            reason: "cache hit would overwrite uncommitted git changes; pass --force to proceed",
        });
    }

    Ok(OverwriteDecision::Write)
}

/// Snapshot of the on-disk file a cache hit is about to overwrite.
#[derive(Debug, Clone)]
pub struct CurrentFile {
    pub file_hash: String,
    pub mode: u32,
    /// Whether the file is modified in the git index or working tree
    /// relative to HEAD. The overwrite policy refuses to clobber
    /// uncommitted work unless `--force` is set.
    pub git_dirty: bool,
}

/// Flags toggling overwrite policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct OverwriteFlags {
    pub force: bool,
    pub no_overwrite: bool,
}

/// What the overwrite policy concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteDecision {
    /// Target file matches incoming content — skip the write.
    NoOp,
    /// Proceed with atomic write.
    Write,
}

// ─── Remote cache push/pull ───

use bytes::Bytes;
use object_store::path::Path as ObjectPath;
use tracing::{debug, info, warn};
use uuid;

use crate::WorkflowStore as Store;

/// Named artifact stores for DVC-compatible `outs.remote` routing.
#[derive(Clone, Default)]
pub struct RemoteArtifactStores {
    named: BTreeMap<String, RemoteArtifactStore>,
    failures: BTreeMap<String, String>,
}

#[derive(Clone)]
struct RemoteArtifactStore {
    store: Arc<Store>,
    prefix: String,
}

#[derive(Clone, Copy)]
struct RemoteArtifactTarget<'a> {
    store: &'a Store,
    prefix: &'a str,
}

impl std::fmt::Debug for RemoteArtifactStores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteArtifactStores")
            .field("named", &self.named.keys().collect::<Vec<_>>())
            .field("failures", &self.failures.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl RemoteArtifactStores {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.named.is_empty() && self.failures.is_empty()
    }

    pub fn insert(
        &mut self,
        name: impl Into<String>,
        store: Arc<Store>,
        prefix: impl Into<String>,
    ) -> Result<()> {
        let name = name.into();
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(CrabError::Configuration {
                key: "workflow.remotes".into(),
                origin: "workflow remote name must not be empty".into(),
            });
        }

        let prefix = prefix.into();
        self.named
            .insert(trimmed.to_owned(), RemoteArtifactStore { store, prefix });
        Ok(())
    }

    pub fn insert_failure(
        &mut self,
        name: impl Into<String>,
        error: impl Into<String>,
    ) -> Result<()> {
        let name = name.into();
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(CrabError::Configuration {
                key: "workflow.remotes".into(),
                origin: "workflow remote name must not be empty".into(),
            });
        }

        self.failures.insert(trimmed.to_owned(), error.into());
        Ok(())
    }

    fn target<'a>(
        &'a self,
        default_store: &'a Store,
        default_prefix: &'a str,
        remote: Option<&str>,
    ) -> Result<RemoteArtifactTarget<'a>> {
        let Some(name) = remote.map(str::trim).filter(|name| !name.is_empty()) else {
            return Ok(RemoteArtifactTarget {
                store: default_store,
                prefix: default_prefix,
            });
        };

        let Some(remote_store) = self.named.get(name) else {
            if let Some(error) = self.failures.get(name) {
                return Err(CrabError::Configuration {
                    key: format!("workflow.remotes.{name}"),
                    origin: format!("workflow remote {name:?} could not be opened: {error}"),
                });
            }
            return Err(CrabError::Configuration {
                key: format!("workflow.remotes.{name}"),
                origin: format!(
                    "output declares remote {name:?} but no matching [workflow.remotes.{name}] is configured"
                ),
            });
        };

        Ok(RemoteArtifactTarget {
            store: remote_store.store.as_ref(),
            prefix: &remote_store.prefix,
        })
    }
}

/// Remote layout paths for workflow cache objects.
///
/// Mirrors the design doc's object layout:
/// ```text
/// {prefix}/workflow/stages/{shard}/{hash}.json
/// {prefix}/workflow/xorbs/{xorb_hash}.xorb
/// {prefix}/refs/crab/stages/{hash}
/// ```
fn remote_manifest_path(prefix: &str, stage_hash: &StageHash) -> ObjectPath {
    let hex = stage_hash.as_hex();
    let shard = &hex[..2];
    ObjectPath::from(format!("{prefix}/workflow/stages/{shard}/{hex}.json"))
}

fn remote_xorb_path(prefix: &str, xorb_hash: &str) -> ObjectPath {
    ObjectPath::from(format!("{prefix}/workflow/xorbs/{xorb_hash}.xorb"))
}

async fn get_remote_bounded(store: &Store, path: &ObjectPath, max_bytes: u64) -> Result<Bytes> {
    let metadata = store.head(path).await?;
    if metadata.size > max_bytes {
        return Err(CrabError::CacheEntryInvalid {
            stage_hash: String::new(),
            detail: format!(
                "remote workflow cache object {} is {} bytes; safety limit is {max_bytes}",
                path, metadata.size
            ),
        });
    }
    let (bytes, _) = store
        .as_storage()
        .get_with_etag_bounded(path, max_bytes)
        .await
        .map_err(CrabError::from)?;
    Ok(bytes)
}

fn remote_ref_path(prefix: &str, stage_hash: &StageHash) -> ObjectPath {
    let hex = stage_hash.as_hex();
    ObjectPath::from(format!("{prefix}/refs/crab/stages/{hex}"))
}

/// Public status/probe path for a remote stage-cache ref.
pub fn remote_stage_ref_path(prefix: &str, stage_hash: &StageHash) -> ObjectPath {
    remote_ref_path(prefix, stage_hash)
}

/// Reject `--cache-push` when `remote_cache_readonly` is configured.
///
/// Call this at the entry point of any push operation. Returns
/// `Err(RemoteCacheReadonly)` when the config flag is set.
pub fn check_remote_cache_readonly(readonly: bool) -> Result<()> {
    if readonly {
        return Err(CrabError::RemoteCacheReadonly);
    }
    Ok(())
}

/// Push a stage cache entry and its output xorbs to the remote store.
///
/// Steps:
/// 1. Upload each output file as a xorb (content-addressed by its blake3 hash).
/// 2. Upload the `StageCacheEntry` JSON manifest.
/// 3. Write a ref at `refs/crab/stages/{hash}` via conditional put (CAS).
///
/// If the ref already exists (concurrent push of the same hash), the
/// second writer detects it and no-ops — the content is identical by
/// construction (same stage hash → same outputs).
///
/// Returns `Ok(true)` if the push wrote new data, `Ok(false)` if the
/// ref already existed (no-op).
pub async fn push_remote(
    store: &Store,
    prefix: &str,
    entry: &StageCacheEntry,
    cache_root: &Path,
) -> Result<bool> {
    push_remote_with_artifact_stores(store, prefix, None, entry, cache_root).await
}

pub async fn push_remote_with_artifact_stores(
    store: &Store,
    prefix: &str,
    artifact_stores: Option<&RemoteArtifactStores>,
    entry: &StageCacheEntry,
    cache_root: &Path,
) -> Result<bool> {
    let fence = store.acquire_gc_writer(prefix).await?;
    let cancel = fence.cancellation();
    let operation = tokio::select! {
        result = push_remote_inner(store, prefix, artifact_stores, entry, cache_root) => result,
        _ = cancel.cancelled() => Err(crate::store::WorkflowGcWriter::lease_lost_error(prefix)),
    };
    let release = fence.release().await;
    match (operation, release) {
        (Ok(wrote), Ok(())) => Ok(wrote),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

async fn push_remote_inner(
    store: &Store,
    prefix: &str,
    artifact_stores: Option<&RemoteArtifactStores>,
    entry: &StageCacheEntry,
    cache_root: &Path,
) -> Result<bool> {
    validate_stage_cache_entry_at(entry, cache_validation_root(cache_root))?;
    if !entry.remote_push_enabled() {
        debug!(
            stage = %entry.stage_name,
            stage_hash = %entry.stage_hash,
            "remote cache push skipped because an output has push=false"
        );
        return Ok(false);
    }

    let stage_hash = &entry.stage_hash;
    let has_named_artifacts = cached_artifacts(entry).any(|out| {
        out.remote
            .as_deref()
            .map(str::trim)
            .is_some_and(|remote| !remote.is_empty())
    });

    // Check if the ref already exists — fast-path no-op for concurrent pushes.
    let ref_path = remote_ref_path(prefix, stage_hash);
    match store.head(&ref_path).await {
        Ok(_) => {
            if has_named_artifacts {
                // Older cache refs may point at manifests whose remote-labeled
                // artifacts were uploaded to the primary. Ensure named stores
                // are populated before treating an existing ref as a hit.
                push_entry_xorbs_remote_with_artifact_stores(
                    store,
                    prefix,
                    artifact_stores,
                    cache_root,
                    entry,
                )
                .await?;
            }
            debug!(
                stage = %entry.stage_name,
                stage_hash = %stage_hash,
                "remote ref already exists; skipping push (concurrent writer won)"
            );
            return Ok(false);
        }
        Err(CrabError::NotFound { .. }) => {
            // Expected — proceed with upload.
        }
        Err(CrabError::Storage(ref e)) if is_not_found_storage(e) => {
            // object_store NotFound variant — proceed.
        }
        Err(e) => return Err(e),
    }

    // Step 1: Upload xorbs for each cached artifact.
    push_entry_xorbs_remote_with_artifact_stores(store, prefix, artifact_stores, cache_root, entry)
        .await?;

    // Step 2: Upload the manifest JSON.
    let manifest_path = remote_manifest_path(prefix, stage_hash);
    let manifest_bytes = canonical_json(entry)?;
    if manifest_bytes.len() > MAX_STAGE_CACHE_ENTRY_BYTES {
        return Err(CrabError::CacheEntryInvalid {
            stage_hash: stage_hash.as_hex(),
            detail: format!(
                "stage cache manifest is {} bytes; safety limit is {MAX_STAGE_CACHE_ENTRY_BYTES}",
                manifest_bytes.len()
            ),
        });
    }
    store
        .put(&manifest_path, Bytes::from(manifest_bytes))
        .await?;
    debug!(
        stage = %entry.stage_name,
        stage_hash = %stage_hash,
        "uploaded stage manifest to remote"
    );

    // Step 3: Write the ref via conditional put (CAS semantics).
    // The ref content is the manifest path — a lightweight pointer.
    let ref_content = Bytes::from(manifest_path.as_ref().as_bytes().to_vec());
    match store.put(&ref_path, ref_content).await {
        Ok(()) => {
            info!(
                stage = %entry.stage_name,
                stage_hash = %stage_hash,
                "published remote cache ref"
            );
            Ok(true)
        }
        Err(CrabError::CasConflict { .. }) => {
            // Another writer beat us — that's fine, content is identical.
            debug!(
                stage = %entry.stage_name,
                stage_hash = %stage_hash,
                "ref CAS conflict; concurrent writer won — no-op"
            );
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

pub(crate) async fn push_entry_xorbs_remote_with_artifact_stores(
    store: &Store,
    prefix: &str,
    artifact_stores: Option<&RemoteArtifactStores>,
    cache_root: &Path,
    entry: &StageCacheEntry,
) -> Result<()> {
    for out in cached_artifacts(entry) {
        let target = artifact_target(artifact_stores, store, prefix, out.remote.as_deref())?;
        if out.kind == OutKind::Directory {
            let Some(manifest) = out.tree_manifest.as_ref() else {
                return Err(CrabError::Internal(format!(
                    "directory output '{}' has no tree manifest for remote cache push",
                    out.path.display()
                )));
            };
            for tree_entry in manifest {
                if tree_entry.kind == "dir" {
                    continue;
                }
                let file_xorb_path = remote_xorb_path(target.prefix, &tree_entry.hash);
                match target.store.head(&file_xorb_path).await {
                    Ok(_) => continue,
                    Err(CrabError::NotFound { .. }) => {}
                    Err(CrabError::Storage(ref e)) if is_not_found_storage(e) => {}
                    Err(e) => return Err(e),
                }
                let local_file = cache_worktree_root(cache_root)
                    .join(&out.path)
                    .join(&tree_entry.path);
                let Some(bytes) = xorb_bytes_for_remote_push(
                    cache_root,
                    &entry.stage_hash,
                    &tree_entry.hash,
                    &local_file,
                )?
                else {
                    return Err(missing_remote_push_xorb(
                        &entry.stage_name,
                        &tree_entry.hash,
                        &local_file,
                    ));
                };
                target
                    .store
                    .put(&file_xorb_path, Bytes::from(bytes))
                    .await?;
                debug!(
                    xorb_hash = %tree_entry.hash,
                    remote = out.remote.as_deref().unwrap_or("default"),
                    "uploaded directory tree xorb to remote"
                );
            }
            continue;
        }

        let xorb_hash = &out.file_hash;
        let xorb_remote = remote_xorb_path(target.prefix, xorb_hash);

        match target.store.head(&xorb_remote).await {
            Ok(_) => {
                debug!(xorb_hash = %xorb_hash, "xorb already exists remotely; skipping");
                continue;
            }
            Err(CrabError::NotFound { .. }) => {}
            Err(CrabError::Storage(ref e)) if is_not_found_storage(e) => {}
            Err(e) => return Err(e),
        }

        let local_path = cache_worktree_root(cache_root).join(&out.path);
        match xorb_bytes_for_remote_push(cache_root, &entry.stage_hash, xorb_hash, &local_path)? {
            Some(bytes) => {
                let size = bytes.len();
                target.store.put(&xorb_remote, Bytes::from(bytes)).await?;
                debug!(
                    xorb_hash = %xorb_hash,
                    size = size,
                    remote = out.remote.as_deref().unwrap_or("default"),
                    "uploaded xorb to remote"
                );
            }
            None => {
                return Err(missing_remote_push_xorb(
                    &entry.stage_name,
                    xorb_hash,
                    &local_path,
                ));
            }
        }
    }

    Ok(())
}

fn artifact_target<'a>(
    artifact_stores: Option<&'a RemoteArtifactStores>,
    default_store: &'a Store,
    default_prefix: &'a str,
    remote: Option<&str>,
) -> Result<RemoteArtifactTarget<'a>> {
    if let Some(stores) = artifact_stores {
        return stores.target(default_store, default_prefix, remote);
    }

    if let Some(name) = remote.map(str::trim).filter(|name| !name.is_empty()) {
        return Err(CrabError::Configuration {
            key: format!("workflow.remotes.{name}"),
            origin: format!(
                "output declares remote {name:?} but no matching [workflow.remotes.{name}] is configured"
            ),
        });
    }

    Ok(RemoteArtifactTarget {
        store: default_store,
        prefix: default_prefix,
    })
}

fn missing_remote_push_xorb(stage_name: &str, xorb_hash: &str, local_path: &Path) -> CrabError {
    CrabError::StageCacheMiss {
        stage: stage_name.to_owned(),
        reason: format!(
            "remote push cannot publish xorb {xorb_hash}: neither local cache bytes nor output file {} are present",
            local_path.display()
        ),
    }
}

fn cache_worktree_root(cache_root: &Path) -> &Path {
    cache_root
        .parent()
        .unwrap_or(cache_root)
        .parent()
        .unwrap_or(cache_root)
}

fn cache_validation_root(cache_root: &Path) -> Option<&Path> {
    let crab_root = cache_root.parent()?;
    if crab_root.file_name().is_some_and(|name| name == ".crab") {
        return crab_root.parent();
    }
    None
}

fn xorb_bytes_for_remote_push(
    cache_root: &Path,
    stage_hash: &StageHash,
    xorb_hash: &str,
    local_path: &Path,
) -> Result<Option<Vec<u8>>> {
    let bytes = if let Some(bytes) = read_local_xorb(cache_root, xorb_hash)? {
        bytes
    } else {
        let metadata = match std::fs::metadata(local_path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(CrabError::Io(e)),
        };
        if metadata.len() > MAX_STAGE_CACHE_ARTIFACT_BYTES {
            return Err(CrabError::CacheEntryCorrupt {
                stage_hash: stage_hash.as_hex(),
                path: local_path.display().to_string(),
                expected: format!("at most {MAX_STAGE_CACHE_ARTIFACT_BYTES} bytes"),
                actual: format!("{} bytes", metadata.len()),
            });
        }
        match read_artifact_file_bounded(local_path, xorb_hash) {
            Ok(bytes) => bytes,
            Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
    };

    let actual = format!("b3:{}", blake3::hash(&bytes).to_hex());
    if actual != xorb_hash {
        return Err(CrabError::CacheEntryCorrupt {
            stage_hash: stage_hash.as_hex(),
            path: local_path.display().to_string(),
            expected: xorb_hash.to_owned(),
            actual,
        });
    }

    Ok(Some(bytes))
}

/// Pull a stage cache entry from the remote store and materialize its artifacts locally.
///
/// Steps:
/// 1. Download the `StageCacheEntry` JSON manifest from the remote.
/// 2. Verify the manifest's `stage_hash` matches the locally-computed hash.
/// 3. Download xorbs for each output and materialize via `write_atomic`.
/// 4. Write the entry to the local cache so subsequent runs hit locally.
///
/// Returns `Ok(Some(entry))` on a successful remote hit, `Ok(None)` when
/// the remote has no entry for this stage hash (404). Network errors and
/// other transient failures are logged at `debug!` and return `Ok(None)`
/// so the caller falls through to local execution.
pub async fn pull_remote(
    store: &Store,
    prefix: &str,
    stage_hash: &StageHash,
    cache_root: &Path,
    working_dir: Option<&Path>,
) -> Result<Option<StageCacheEntry>> {
    pull_remote_with_artifact_stores(store, prefix, None, stage_hash, cache_root, working_dir).await
}

pub async fn pull_remote_with_artifact_stores(
    store: &Store,
    prefix: &str,
    artifact_stores: Option<&RemoteArtifactStores>,
    stage_hash: &StageHash,
    cache_root: &Path,
    working_dir: Option<&Path>,
) -> Result<Option<StageCacheEntry>> {
    // Step 1: Download the manifest.
    let manifest_path = remote_manifest_path(prefix, stage_hash);
    let manifest_bytes =
        match get_remote_bounded(store, &manifest_path, MAX_STAGE_CACHE_ENTRY_BYTES as u64).await {
            Ok(bytes) => bytes,
            Err(CrabError::NotFound { .. }) => {
                debug!(stage_hash = %stage_hash, "remote cache miss: manifest not found");
                return Ok(None);
            }
            Err(CrabError::Storage(ref e)) if is_not_found_storage(e) => {
                debug!(stage_hash = %stage_hash, "remote cache miss: manifest not found (storage)");
                return Ok(None);
            }
            Err(e) => {
                debug!(
                    stage_hash = %stage_hash,
                    error = %e,
                    "remote cache pull failed during manifest download"
                );
                return Ok(None);
            }
        };

    // Step 2: Parse and verify the manifest.
    let entry: StageCacheEntry = match serde_json::from_slice(&manifest_bytes) {
        Ok(e) => e,
        Err(e) => {
            debug!(
                stage_hash = %stage_hash,
                error = %e,
                "remote cache pull: manifest parse failed"
            );
            return Ok(None);
        }
    };

    // Verify the manifest's stage_hash matches what we computed locally.
    if entry.stage_hash != *stage_hash {
        return Err(CrabError::CacheEntryHashMismatch {
            manifest_hash: entry.stage_hash.as_hex(),
            local_hash: stage_hash.as_hex(),
        });
    }

    // Verify schema version is supported.
    if entry.schema_version > ENTRY_SCHEMA_MAX_SUPPORTED {
        debug!(
            stage_hash = %stage_hash,
            found = entry.schema_version,
            supported = ENTRY_SCHEMA_MAX_SUPPORTED,
            "remote cache pull: unsupported schema version"
        );
        return Ok(None);
    }
    validate_stage_cache_entry(&entry)?;

    // Step 3: Download xorbs and materialize artifacts with blake3 verification.
    let base_dir = working_dir.unwrap_or_else(|| std::path::Path::new("."));
    let run_id = uuid::Uuid::now_v7();

    // Track materialized paths for cleanup on verification failure.
    let mut materialized_paths: Vec<PathBuf> = Vec::new();

    for out in cached_artifacts(&entry) {
        let target = artifact_target(artifact_stores, store, prefix, out.remote.as_deref())?;
        match out.kind {
            OutKind::File | OutKind::Stdout => {
                let xorb_path = remote_xorb_path(target.prefix, &out.file_hash);
                let xorb_bytes = match get_remote_bounded(
                    target.store,
                    &xorb_path,
                    MAX_STAGE_CACHE_ARTIFACT_BYTES,
                )
                .await
                {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        debug!(
                            stage_hash = %stage_hash,
                            xorb_hash = %out.file_hash,
                            error = %e,
                            "remote cache pull: xorb download failed"
                        );
                        cleanup_partial(&materialized_paths);
                        return Ok(None);
                    }
                };

                // Verify blake3 hash of downloaded bytes against manifest.
                let actual_hash = format!("b3:{}", blake3::hash(&xorb_bytes).to_hex());
                if actual_hash != out.file_hash {
                    cleanup_partial(&materialized_paths);
                    return Err(CrabError::CacheEntryCorrupt {
                        stage_hash: stage_hash.as_hex(),
                        path: out.path.display().to_string(),
                        expected: out.file_hash.clone(),
                        actual: actual_hash,
                    });
                }
                if xorb_bytes.len() as u64 != out.size {
                    cleanup_partial(&materialized_paths);
                    return Err(CrabError::CacheEntryCorrupt {
                        stage_hash: stage_hash.as_hex(),
                        path: out.path.display().to_string(),
                        expected: format!("{} bytes", out.size),
                        actual: format!("{} bytes", xorb_bytes.len()),
                    });
                }
                write_local_xorb(cache_root, &out.file_hash, &xorb_bytes)?;

                let target = if out.path.is_absolute() {
                    out.path.clone()
                } else {
                    base_dir.join(&out.path)
                };
                crate::materialize::write_atomic(&target, &xorb_bytes, run_id, out.mode)?;
                materialized_paths.push(target);
            }
            OutKind::Directory => {
                if let Some(ref manifest) = out.tree_manifest {
                    // Download each file entry from the remote and materialize.
                    let local_target = if out.path.is_absolute() {
                        out.path.clone()
                    } else {
                        base_dir.join(&out.path)
                    };

                    for tree_entry in manifest {
                        if tree_entry.kind == "dir" {
                            continue;
                        }
                        let file_xorb_path = remote_xorb_path(target.prefix, &tree_entry.hash);
                        let file_bytes = match get_remote_bounded(
                            target.store,
                            &file_xorb_path,
                            MAX_STAGE_CACHE_ARTIFACT_BYTES,
                        )
                        .await
                        {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                debug!(
                                    stage_hash = %stage_hash,
                                    tree_path = %tree_entry.path,
                                    error = %e,
                                    "remote cache pull: tree entry xorb download failed"
                                );
                                cleanup_partial(&materialized_paths);
                                return Ok(None);
                            }
                        };

                        // Verify blake3 hash of each tree entry file.
                        let actual_hash = format!("b3:{}", blake3::hash(&file_bytes).to_hex());
                        if actual_hash != tree_entry.hash {
                            cleanup_partial(&materialized_paths);
                            return Err(CrabError::CacheEntryCorrupt {
                                stage_hash: stage_hash.as_hex(),
                                path: format!("{}/{}", out.path.display(), tree_entry.path),
                                expected: tree_entry.hash.clone(),
                                actual: actual_hash,
                            });
                        }
                        if file_bytes.len() as u64 != tree_entry.size {
                            cleanup_partial(&materialized_paths);
                            return Err(CrabError::CacheEntryCorrupt {
                                stage_hash: stage_hash.as_hex(),
                                path: format!("{}/{}", out.path.display(), tree_entry.path),
                                expected: format!("{} bytes", tree_entry.size),
                                actual: format!("{} bytes", file_bytes.len()),
                            });
                        }
                        write_local_xorb(cache_root, &tree_entry.hash, &file_bytes)?;
                    }

                    crate::materialize::materialize_directory(
                        &local_target,
                        manifest,
                        cache_root,
                        run_id,
                    )?;
                    materialized_paths.push(local_target);
                } else {
                    debug!(
                        stage_hash = %stage_hash,
                        path = %out.path.display(),
                        "remote cache pull: directory out has no tree manifest"
                    );
                    cleanup_partial(&materialized_paths);
                    return Ok(None);
                }
            }
        }
    }

    // Step 4: Write the entry to the local cache for future local hits.
    if let Err(e) = write_local(cache_root, &entry) {
        debug!(
            stage_hash = %stage_hash,
            error = %e,
            "remote cache pull: failed to write local cache entry (non-fatal)"
        );
    }

    info!(
        stage = %entry.stage_name,
        stage_hash = %stage_hash,
        "remote cache pull: materialized outputs from remote"
    );
    Ok(Some(entry))
}

/// Remove partially materialized files on verification failure.
fn cleanup_partial(paths: &[PathBuf]) {
    for path in paths {
        if path.is_dir() {
            if let Err(e) = std::fs::remove_dir_all(path) {
                debug!(
                    path = %path.display(),
                    error = %e,
                    "cleanup_partial: failed to remove directory"
                );
            }
        } else if let Err(e) = std::fs::remove_file(path) {
            debug!(
                path = %path.display(),
                error = %e,
                "cleanup_partial: failed to remove file"
            );
        }
    }
}

/// Scan all local stage cache entries and push any that lack a
/// corresponding remote ref. Used by `crab workflow push-cache --all`.
pub async fn push_all_local(
    store: &Store,
    prefix: &str,
    cache_root: &Path,
) -> Result<PushAllResult> {
    push_all_local_with_artifact_stores_and_cancel(
        store,
        prefix,
        None,
        cache_root,
        &CancellationToken::new(),
    )
    .await
}

pub async fn push_all_local_with_artifact_stores(
    store: &Store,
    prefix: &str,
    artifact_stores: Option<&RemoteArtifactStores>,
    cache_root: &Path,
) -> Result<PushAllResult> {
    push_all_local_with_artifact_stores_and_cancel(
        store,
        prefix,
        artifact_stores,
        cache_root,
        &CancellationToken::new(),
    )
    .await
}

/// Scan and push local stage cache entries while honoring cancellation.
pub async fn push_all_local_with_artifact_stores_and_cancel(
    store: &Store,
    prefix: &str,
    artifact_stores: Option<&RemoteArtifactStores>,
    cache_root: &Path,
    cancel: &CancellationToken,
) -> Result<PushAllResult> {
    const MAX_LOCAL_STAGE_CACHE_ENTRIES: usize = 1_000_000;

    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }
    let stages_dir = cache_root.join("stages");
    let mut pushed = 0u32;
    let mut skipped = 0u32;
    let mut errors = 0u32;

    if !stages_dir.exists() {
        return Ok(PushAllResult {
            pushed,
            skipped,
            errors,
        });
    }

    // Walk the 2-char shard directories.
    let shards = std::fs::read_dir(&stages_dir).map_err(CrabError::Io)?;
    let mut scanned_entries = 0usize;
    for shard_entry in shards {
        if cancel.is_cancelled() {
            return Err(CrabError::Cancelled);
        }
        let shard_entry = shard_entry.map_err(CrabError::Io)?;
        let shard_path = shard_entry.path();
        if !shard_path.is_dir() {
            continue;
        }

        let files = std::fs::read_dir(&shard_path).map_err(CrabError::Io)?;
        for file_entry in files {
            if cancel.is_cancelled() {
                return Err(CrabError::Cancelled);
            }
            let file_entry = file_entry.map_err(CrabError::Io)?;
            let file_path = file_entry.path();
            let Some(ext) = file_path.extension() else {
                continue;
            };
            if ext != "json" {
                continue;
            }
            scanned_entries = scanned_entries.saturating_add(1);
            if scanned_entries > MAX_LOCAL_STAGE_CACHE_ENTRIES {
                return Err(CrabError::Configuration {
                    key: "workflow cache entry count".to_owned(),
                    origin: format!(
                        "local stage cache contains more than {MAX_LOCAL_STAGE_CACHE_ENTRIES} entries"
                    ),
                });
            }

            // Read and parse the entry.
            let bytes = match read_stage_cache_file_bounded(&file_path, "") {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!(path = %file_path.display(), error = %error, "failed to read local stage cache entry");
                    errors = errors.saturating_add(1);
                    continue;
                }
            };
            let entry: StageCacheEntry = match serde_json::from_slice(&bytes) {
                Ok(e) => e,
                Err(error) => {
                    warn!(path = %file_path.display(), error = %error, "failed to parse local stage cache entry");
                    errors = errors.saturating_add(1);
                    continue;
                }
            };

            if let Err(error) =
                validate_stage_cache_entry_at(&entry, cache_validation_root(cache_root))
            {
                warn!(path = %file_path.display(), error = %error, "local stage cache entry failed validation");
                errors = errors.saturating_add(1);
                continue;
            }

            if !entry.remote_push_enabled() {
                skipped = skipped.saturating_add(1);
                continue;
            }

            match tokio::select! {
                result = push_remote_with_artifact_stores(
                    store,
                    prefix,
                    artifact_stores,
                    &entry,
                    cache_root,
                ) => result,
                () = cancel.cancelled() => return Err(CrabError::Cancelled),
            } {
                Ok(true) => pushed = pushed.saturating_add(1),
                Ok(false) => skipped = skipped.saturating_add(1),
                Err(e) => {
                    warn!(
                        stage = %entry.stage_name,
                        error = %e,
                        "failed to push stage cache entry to remote"
                    );
                    errors = errors.saturating_add(1);
                }
            }
        }
    }

    Ok(PushAllResult {
        pushed,
        skipped,
        errors,
    })
}

/// Result of a `push-cache --all` operation.
#[derive(Debug, Clone, Serialize)]
pub struct PushAllResult {
    pub pushed: u32,
    pub skipped: u32,
    pub errors: u32,
}

/// Check if an `object_store::Error` is a NotFound variant.
fn is_not_found_storage(e: &object_store::Error) -> bool {
    matches!(e, object_store::Error::NotFound { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn sample_entry(stage_hash: StageHash) -> StageCacheEntry {
        StageCacheEntry {
            schema_version: ENTRY_SCHEMA_VERSION,
            stage_hash,
            stage_name: "train".to_owned(),
            cmd: CachedCmd::Shell {
                shell: "python train.py".to_owned(),
            },
            outs: vec![CachedOut {
                path: PathBuf::from("out/model.pkl"),
                kind: OutKind::File,
                push: true,
                remote: None,
                file_hash: format!("b3:{}", "ab".repeat(32)),
                size: 42,
                mode: 0o644,
                tree_manifest: None,
            }],
            metrics: vec![],
            plots: vec![],
            executed_at: "2026-01-01T00:00:00.000Z".to_owned(),
            duration_ms: 1000,
            exec_id: None,
            attempts: 1,
            host_fingerprint: "linux-x86_64-crab-0.8.0".to_owned(),
        }
    }

    #[test]
    fn cached_out_push_defaults_true_when_missing() {
        let entry = sample_entry(StageHash([1; 32]));
        let text = serde_json::to_string(&entry).unwrap();
        assert!(!text.contains("\"push\""));

        let parsed: StageCacheEntry = serde_json::from_str(&text).unwrap();
        assert!(parsed.outs[0].push);
        assert!(parsed.remote_push_enabled());
    }

    #[test]
    fn cache_entry_remote_push_disabled_by_local_only_out() {
        let mut entry = sample_entry(StageHash([2; 32]));
        entry.outs[0].push = false;
        assert!(!entry.remote_push_enabled());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_all_skips_local_only_entries() {
        let tmp = TempDir::new().unwrap();
        let mut entry = sample_entry(StageHash([7; 32]));
        entry.outs[0].push = false;
        write_local(tmp.path(), &entry).unwrap();

        let store = crate::WorkflowStore::new(Arc::new(object_store::memory::InMemory::new()));
        let result = push_all_local(&store, "org/repo", tmp.path())
            .await
            .unwrap();

        assert_eq!(result.pushed, 0);
        assert_eq!(result.skipped, 1);
        assert_eq!(result.errors, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_all_reports_malformed_local_entries() {
        let tmp = TempDir::new().unwrap();
        let shard = tmp.path().join("stages/aa");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join("broken.json"), b"not json").unwrap();
        let store = crate::WorkflowStore::new(Arc::new(object_store::memory::InMemory::new()));

        let result = push_all_local(&store, "org/repo", tmp.path())
            .await
            .unwrap();

        assert_eq!(result.errors, 1);
        assert_eq!(result.pushed, 0);
    }

    #[test]
    fn read_missing_returns_none() {
        let tmp = TempDir::new().unwrap();
        let got = read_local(tmp.path(), &StageHash([0; 32])).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn local_xorb_path_rejects_malformed_hashes() {
        let tmp = TempDir::new().unwrap();
        let error = read_local_xorb(tmp.path(), "../escape").unwrap_err();
        assert!(matches!(error, CrabError::Internal(_)));
    }

    #[test]
    fn local_artifact_read_rejects_oversized_file_before_consuming_it() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oversized-artifact");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_STAGE_CACHE_ARTIFACT_BYTES + 1)
            .unwrap();

        let error = read_artifact_file_bounded(&path, "b3:expected").unwrap_err();

        assert!(matches!(error, CrabError::CacheEntryCorrupt { .. }));
    }

    #[test]
    fn write_then_read_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let hash = StageHash([1; 32]);
        let entry = sample_entry(hash);

        write_local(tmp.path(), &entry).unwrap();
        let got = read_local(tmp.path(), &hash).unwrap().unwrap();
        assert_eq!(got, entry);
    }

    #[test]
    fn cached_out_remote_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let hash = StageHash([6; 32]);
        let mut entry = sample_entry(hash);
        entry.outs[0].remote = Some("cold-storage".to_owned());

        write_local(tmp.path(), &entry).unwrap();
        let got = read_local(tmp.path(), &hash).unwrap().unwrap();

        assert_eq!(got.outs[0].remote.as_deref(), Some("cold-storage"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn named_output_remote_routes_artifact_xorbs() {
        let primary = Arc::new(crate::WorkflowStore::new(Arc::new(
            object_store::memory::InMemory::new(),
        )));
        let named = Arc::new(crate::WorkflowStore::new(Arc::new(
            object_store::memory::InMemory::new(),
        )));
        let mut routes = RemoteArtifactStores::default();
        routes
            .insert("models", named.clone(), "artifact/repo")
            .unwrap();

        let push_tmp = TempDir::new().unwrap();
        let stage_hash = StageHash([8; 32]);
        let bytes = b"model bytes";
        let file_hash = format!("b3:{}", blake3::hash(bytes).to_hex());
        let mut entry = sample_entry(stage_hash);
        entry.outs[0].remote = Some("models".to_owned());
        entry.outs[0].file_hash = file_hash.clone();
        entry.outs[0].size = bytes.len() as u64;
        write_local_xorb(push_tmp.path(), &file_hash, bytes).unwrap();

        push_remote_with_artifact_stores(
            primary.as_ref(),
            "org/repo",
            Some(&routes),
            &entry,
            push_tmp.path(),
        )
        .await
        .unwrap();

        primary
            .head(&remote_manifest_path("org/repo", &stage_hash))
            .await
            .unwrap();
        assert!(
            primary
                .head(&remote_xorb_path("org/repo", &file_hash))
                .await
                .is_err()
        );
        named
            .head(&remote_xorb_path("artifact/repo", &file_hash))
            .await
            .unwrap();

        let pull_tmp = TempDir::new().unwrap();
        let pulled = pull_remote_with_artifact_stores(
            primary.as_ref(),
            "org/repo",
            Some(&routes),
            &stage_hash,
            pull_tmp.path(),
            Some(pull_tmp.path()),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(pulled.outs[0].remote.as_deref(), Some("models"));
        assert_eq!(
            std::fs::read(pull_tmp.path().join("out/model.pkl")).unwrap(),
            bytes
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn named_output_remote_backfills_artifact_when_ref_already_exists() {
        let primary = Arc::new(crate::WorkflowStore::new(Arc::new(
            object_store::memory::InMemory::new(),
        )));
        let named = Arc::new(crate::WorkflowStore::new(Arc::new(
            object_store::memory::InMemory::new(),
        )));
        let mut routes = RemoteArtifactStores::default();
        routes
            .insert("models", named.clone(), "artifact/repo")
            .unwrap();

        let tmp = TempDir::new().unwrap();
        let stage_hash = StageHash([10; 32]);
        let bytes = b"model bytes";
        let file_hash = format!("b3:{}", blake3::hash(bytes).to_hex());
        let mut entry = sample_entry(stage_hash);
        entry.outs[0].remote = Some("models".to_owned());
        entry.outs[0].file_hash = file_hash.clone();
        write_local_xorb(tmp.path(), &file_hash, bytes).unwrap();

        let manifest_path = remote_manifest_path("org/repo", &stage_hash);
        primary
            .put(&manifest_path, Bytes::from(canonical_json(&entry).unwrap()))
            .await
            .unwrap();
        primary
            .put(
                &remote_ref_path("org/repo", &stage_hash),
                Bytes::from(manifest_path.as_ref().as_bytes().to_vec()),
            )
            .await
            .unwrap();

        let wrote = push_remote_with_artifact_stores(
            primary.as_ref(),
            "org/repo",
            Some(&routes),
            &entry,
            tmp.path(),
        )
        .await
        .unwrap();

        assert!(!wrote);
        named
            .head(&remote_xorb_path("artifact/repo", &file_hash))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn named_output_remote_requires_configured_route() {
        let primary = crate::WorkflowStore::new(Arc::new(object_store::memory::InMemory::new()));
        let tmp = TempDir::new().unwrap();
        let bytes = b"model bytes";
        let file_hash = format!("b3:{}", blake3::hash(bytes).to_hex());
        let mut entry = sample_entry(StageHash([9; 32]));
        entry.outs[0].remote = Some("models".to_owned());
        entry.outs[0].file_hash = file_hash.clone();
        write_local_xorb(tmp.path(), &file_hash, bytes).unwrap();

        let error = push_remote(&primary, "org/repo", &entry, tmp.path())
            .await
            .unwrap_err();

        match error {
            CrabError::Configuration { key, origin } => {
                assert_eq!(key, "workflow.remotes.models");
                assert!(origin.contains("output declares remote"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_cache_rejects_traversal_before_materialization() {
        let primary = crate::WorkflowStore::new(Arc::new(object_store::memory::InMemory::new()));
        let tmp = TempDir::new().unwrap();
        let stage_hash = StageHash([12; 32]);
        let mut entry = sample_entry(stage_hash);
        entry.outs[0].path = PathBuf::from("../escape.txt");
        let manifest_path = remote_manifest_path("org/repo", &stage_hash);
        primary
            .put(&manifest_path, Bytes::from(canonical_json(&entry).unwrap()))
            .await
            .unwrap();

        let error = pull_remote(
            &primary,
            "org/repo",
            &stage_hash,
            tmp.path(),
            Some(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, CrabError::CacheEntryInvalid { .. }));
        assert!(!tmp.path().join("escape.txt").exists());
    }

    #[test]
    fn canonical_json_has_sorted_keys() {
        let tmp = TempDir::new().unwrap();
        let hash = StageHash([2; 32]);
        let entry = sample_entry(hash);
        write_local(tmp.path(), &entry).unwrap();

        let bytes = std::fs::read(entry_path(tmp.path(), &hash)).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Top-level keys must appear in alphabetical order.
        let attempts = s.find("\"attempts\"").unwrap();
        let duration = s.find("\"duration_ms\"").unwrap();
        let stage_hash = s.find("\"stage_hash\"").unwrap();
        assert!(attempts < duration, "attempts should precede duration_ms");
        assert!(
            duration < stage_hash,
            "duration_ms should precede stage_hash"
        );
    }

    #[test]
    fn newer_schema_is_refused() {
        let tmp = TempDir::new().unwrap();
        let hash = StageHash([3; 32]);
        let path = entry_path(tmp.path(), &hash);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Stamp a future schema version on disk.
        std::fs::write(&path, br#"{"schema_version":999,"stage_hash":{"0":[0]}}"#).unwrap();

        let err = read_local(tmp.path(), &hash).unwrap_err();
        assert!(matches!(err, CrabError::CacheEntrySchemaNewer { found, .. } if found == 999));
    }

    #[test]
    fn corrupt_json_yields_corrupt_error() {
        let tmp = TempDir::new().unwrap();
        let hash = StageHash([4; 32]);
        let path = entry_path(tmp.path(), &hash);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        let err = read_local(tmp.path(), &hash).unwrap_err();
        assert!(matches!(err, CrabError::WorkflowJournalCorrupt { .. }));
    }

    #[test]
    fn overwrite_policy_allows_write_when_target_absent() {
        let decision = overwrite_policy(
            "train",
            Path::new("out.txt"),
            &CachedOut {
                path: PathBuf::from("out.txt"),
                kind: OutKind::File,
                push: true,
                remote: None,
                file_hash: "b3:aa".to_owned(),
                size: 1,
                mode: 0o644,
                tree_manifest: None,
            },
            None,
            OverwriteFlags::default(),
        )
        .unwrap();
        assert_eq!(decision, OverwriteDecision::Write);
    }

    #[test]
    fn overwrite_policy_no_op_when_hash_matches() {
        let incoming = CachedOut {
            path: PathBuf::from("out.txt"),
            kind: OutKind::File,
            push: true,
            remote: None,
            file_hash: "b3:aa".to_owned(),
            size: 1,
            mode: 0o644,
            tree_manifest: None,
        };
        let current = CurrentFile {
            file_hash: "b3:aa".to_owned(),
            mode: 0o644,
            git_dirty: false,
        };
        let decision = overwrite_policy(
            "train",
            Path::new("out.txt"),
            &incoming,
            Some(&current),
            OverwriteFlags::default(),
        )
        .unwrap();
        assert_eq!(decision, OverwriteDecision::NoOp);
    }

    #[test]
    fn overwrite_policy_refuses_hash_mismatch_with_no_overwrite() {
        let incoming = CachedOut {
            path: PathBuf::from("out.txt"),
            kind: OutKind::File,
            push: true,
            remote: None,
            file_hash: "b3:aa".to_owned(),
            size: 1,
            mode: 0o644,
            tree_manifest: None,
        };
        let current = CurrentFile {
            file_hash: "b3:bb".to_owned(),
            mode: 0o644,
            git_dirty: false,
        };
        let err = overwrite_policy(
            "train",
            Path::new("out.txt"),
            &incoming,
            Some(&current),
            OverwriteFlags {
                force: false,
                no_overwrite: true,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CrabError::StageOverwriteConflict { .. }));
    }

    #[test]
    fn overwrite_policy_refuses_dirty_git_without_force() {
        let incoming = CachedOut {
            path: PathBuf::from("out.txt"),
            kind: OutKind::File,
            push: true,
            remote: None,
            file_hash: "b3:aa".to_owned(),
            size: 1,
            mode: 0o644,
            tree_manifest: None,
        };
        let current = CurrentFile {
            file_hash: "b3:bb".to_owned(),
            mode: 0o644,
            git_dirty: true,
        };
        let err = overwrite_policy(
            "train",
            Path::new("out.txt"),
            &incoming,
            Some(&current),
            OverwriteFlags::default(),
        )
        .unwrap_err();
        assert!(matches!(err, CrabError::StageOverwriteConflict { .. }));
    }

    #[test]
    fn overwrite_policy_allows_dirty_git_with_force() {
        let incoming = CachedOut {
            path: PathBuf::from("out.txt"),
            kind: OutKind::File,
            push: true,
            remote: None,
            file_hash: "b3:aa".to_owned(),
            size: 1,
            mode: 0o644,
            tree_manifest: None,
        };
        let current = CurrentFile {
            file_hash: "b3:bb".to_owned(),
            mode: 0o644,
            git_dirty: true,
        };
        let decision = overwrite_policy(
            "train",
            Path::new("out.txt"),
            &incoming,
            Some(&current),
            OverwriteFlags {
                force: true,
                no_overwrite: false,
            },
        )
        .unwrap();
        assert_eq!(decision, OverwriteDecision::Write);
    }
}
