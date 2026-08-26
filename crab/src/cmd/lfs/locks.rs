//! `crab lfs lock`, `crab lfs unlock`, `crab lfs locks` — advisory file locking.
//!
//! Wires the CLI lock subcommands to [`crate::lfs::lock::LockManager`]
//! via the shared [`super::store_setup`] module.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::lfs::lock::{LockManager, LockRecord};
use fs4::fs_std::FileExt as LockFileExt;
use serde::Serialize;

use super::store_setup::{git_user_identity, resolve_lfs_remote_for_operation_with_remote_sync};

#[derive(Debug, Clone, Default)]
pub struct LfsLockOptions {
    pub path: String,
    pub remote: Option<String>,
    pub json: bool,
    pub expires_in: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LfsUnlockOptions {
    pub path: Option<String>,
    pub remote: Option<String>,
    pub force: bool,
    pub id: Option<String>,
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct LfsLocksOptions {
    pub remote: Option<String>,
    pub id: Option<String>,
    pub path: Option<String>,
    pub local: bool,
    pub cached: bool,
    pub mode: OutputMode,
    pub verify: bool,
    pub limit: Option<usize>,
}

/// Run `crab lfs lock <path> [--expires-in <duration>]`.
///
/// Creates an advisory lock on the given path. The lock owner is
/// determined from `git config user.email` (falling back to
/// `git config user.name`).
pub async fn run_lfs_lock(options: LfsLockOptions) -> Result<()> {
    let path = canonical_lock_path(&options.path, "lfs lock", true)?;
    let ctx = resolve_lfs_remote_for_operation_with_remote_sync("lock", options.remote.as_deref())?;
    let owner = git_user_identity()?;

    let expires_dur = options
        .expires_in
        .as_deref()
        .map(parse_duration)
        .transpose()?;

    let lock_store = crate::storage::Store::from_storage(ctx.store.store().clone());
    let mgr = LockManager::lfs(lock_store, &ctx.prefix);
    let record = mgr.lock_with_expiry(&path, &owner, expires_dur).await?;
    LockCache::open(&ctx.local_lfs_dir, options.remote.as_deref())?.add_local(&record)?;
    if options.json {
        emit_json("lfs.lock", "1.1", &record);
    } else if let Some(exp) = record.expires_at {
        println!(
            "Locked {} (id: {}, expires at Unix {})",
            record.path, record.id, exp
        );
    } else {
        println!("Locked {} (id: {})", record.path, record.id);
    }
    Ok(())
}

/// Run `crab lfs unlock <path> [--force]`.
///
/// Releases the advisory lock for the given path. When `force` is true
/// the lock is released regardless of the current owner.
pub async fn run_lfs_unlock(options: LfsUnlockOptions) -> Result<()> {
    validate_unlock_options(&options)?;
    let ctx =
        resolve_lfs_remote_for_operation_with_remote_sync("unlock", options.remote.as_deref())?;

    let lock_store = crate::storage::Store::from_storage(ctx.store.store().clone());
    let mgr = LockManager::lfs(lock_store, &ctx.prefix);
    let (path, id) = unlock_target(&mgr, &options).await?;
    let path = normalize_lock_path(&path, "lfs unlock")?;

    if options.force {
        mgr.force_unlock(&path).await?;
    } else {
        validate_unlock_worktree_path(&path)?;
        let owner = git_user_identity()?;
        mgr.unlock_with_id(&path, &owner, id.as_deref()).await?;
    }
    let cache = LockCache::open(&ctx.local_lfs_dir, options.remote.as_deref())?;
    if let Some(id) = &id {
        cache.remove_local_by_id(id)?;
    } else {
        cache.remove_local_by_path(&path)?;
    }

    if options.json {
        emit_json(
            "lfs.unlock",
            "1.1",
            &serde_json::json!({ "path": path, "id": id, "unlocked": true }),
        );
    } else if let Some(id) = id {
        println!("Unlocked Lock {id}");
    } else {
        println!("Unlocked {path}");
    }
    Ok(())
}

fn validate_unlock_options(options: &LfsUnlockOptions) -> Result<()> {
    if options.path.is_some() == options.id.is_some() {
        return Err(CrabError::Configuration {
            key: "lfs unlock".to_owned(),
            origin: "Exactly one of --id or a path must be provided".to_owned(),
        });
    }
    Ok(())
}

fn canonical_lock_path(path: &str, key: &str, require_file: bool) -> Result<String> {
    let normalized = normalize_lock_path(path, key)?;
    if !require_file {
        return Ok(normalized);
    }

    validate_worktree_file(path, key)?;
    let root = std::env::current_dir()
        .and_then(|root| root.canonicalize())
        .map_err(|error| CrabError::Configuration {
            key: key.to_owned(),
            origin: format!("failed to resolve the working tree: {error}"),
        })?;
    let resolved =
        root.join(&normalized)
            .canonicalize()
            .map_err(|error| CrabError::Configuration {
                key: key.to_owned(),
                origin: format!("failed to resolve path \"{path}\": {error}"),
            })?;
    if !resolved.starts_with(&root) {
        return Err(CrabError::Configuration {
            key: key.to_owned(),
            origin: format!("path \"{path}\" resolves outside the working tree"),
        });
    }
    Ok(normalized)
}

fn normalize_lock_path(path: &str, key: &str) -> Result<String> {
    let mut components = Vec::new();
    for component in Path::new(path).components() {
        match component {
            std::path::Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| CrabError::Configuration {
                    key: key.to_owned(),
                    origin: format!("path \"{path}\" is not valid UTF-8"),
                })?;
                components.push(value.to_owned());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(CrabError::Configuration {
                    key: key.to_owned(),
                    origin: format!("path \"{path}\" must be repository-relative"),
                });
            }
        }
    }
    if components.is_empty() {
        return Err(CrabError::Configuration {
            key: key.to_owned(),
            origin: "lock path must name a file".to_owned(),
        });
    }
    Ok(components.join("/"))
}

fn validate_unlock_worktree_path(path: &str) -> Result<()> {
    validate_worktree_file(path, "lfs unlock")?;
    if !git_status_clean_for_path(path)? {
        return Err(CrabError::Configuration {
            key: "lfs unlock".to_owned(),
            origin: format!("path \"{path}\" must have a clean git status; use --force to skip"),
        });
    }
    Ok(())
}

fn validate_worktree_file(path: &str, key: &str) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|e| CrabError::Configuration {
        key: key.to_owned(),
        origin: format!("path \"{path}\" must exist in the working tree: {e}"),
    })?;

    if !metadata.is_file() {
        return Err(CrabError::Configuration {
            key: key.to_owned(),
            origin: format!("path \"{path}\" is not a file"),
        });
    }

    Ok(())
}

fn git_status_clean_for_path(path: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "--", path])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git status: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!("git status failed: {stderr}")));
    }

    Ok(output.stdout.is_empty())
}

async fn unlock_target(
    mgr: &LockManager,
    options: &LfsUnlockOptions,
) -> Result<(String, Option<String>)> {
    if let Some(path) = options.path.clone() {
        return Ok((path, None));
    }

    let Some(id) = options.id.clone() else {
        return Err(CrabError::Configuration {
            key: "lfs unlock".to_owned(),
            origin: "Exactly one of --id or a path must be provided".to_owned(),
        });
    };
    let record = lock_by_id(mgr, &id).await?;
    Ok((record.path, Some(id)))
}

async fn lock_by_id(mgr: &LockManager, id: &str) -> Result<LockRecord> {
    mgr.list()
        .await?
        .into_iter()
        .find(|lock| lock.id == id)
        .ok_or_else(|| CrabError::NotFound {
            path: format!("LFS lock id {id}"),
        })
}

/// Run `crab lfs locks [--json] [--verify]`.
///
/// Lists all active advisory locks. When `json` is true the output is
/// stable JSON suitable for scripting. When `verify` is true, marks locks
/// owned by the current Git identity and reports stale local lock records.
pub async fn run_lfs_locks(options: LfsLocksOptions) -> Result<()> {
    validate_locks_options(&options)?;
    if options.local || options.cached {
        let cache = LockCache::open(&resolve_local_lfs_dir()?, options.remote.as_deref())?;
        let locks = if options.local {
            filter_locks(cache.read_local()?, &options)
        } else {
            cache.read_remote()?
        };
        emit_locks(&locks, options.mode);
        return Ok(());
    }

    let ctx =
        resolve_lfs_remote_for_operation_with_remote_sync("locks", options.remote.as_deref())?;
    let lock_store = crate::storage::Store::from_storage(ctx.store.store().clone());
    let mgr = LockManager::lfs(lock_store, &ctx.prefix);
    let cache = LockCache::open(&ctx.local_lfs_dir, options.remote.as_deref())?;

    if options.verify {
        let invalid = mgr.verify_locks().await?;
        if !invalid.is_empty() {
            eprintln!("{} invalid lock record(s) found:", invalid.len());
            for key in &invalid {
                eprintln!("  {key}");
            }
            return Err(CrabError::Internal(format!(
                "{} invalid lock record(s) detected",
                invalid.len()
            )));
        }
        let remote_locks = mgr.list().await?;
        let local_locks = cache.read_local()?;
        let owner = git_user_identity().unwrap_or_default();
        let verified = filter_verified_locks(
            verified_lock_records(&remote_locks, &local_locks, &owner),
            &options,
        );
        cache.write_remote(&remote_locks)?;
        cache.replace_local_owned(&remote_locks, &owner)?;
        emit_verified_locks(&verified, options.mode);
        return Ok(());
    }

    let locks = mgr.list().await?;
    if options.id.is_none() && options.path.is_none() && options.limit.unwrap_or(0) == 0 {
        cache.write_remote(&locks)?;
    }
    let locks = filter_locks(locks, &options);

    emit_locks(&locks, options.mode);

    Ok(())
}

fn validate_locks_options(options: &LfsLocksOptions) -> Result<()> {
    if options.cached && options.limit.unwrap_or(0) > 0 {
        return Err(CrabError::Configuration {
            key: "lfs locks --cached".to_owned(),
            origin: "cannot combine --cached with --limit".to_owned(),
        });
    }
    if options.cached && (options.id.is_some() || options.path.is_some()) {
        return Err(CrabError::Configuration {
            key: "lfs locks --cached".to_owned(),
            origin: "cannot combine --cached with --id or --path filters".to_owned(),
        });
    }
    if options.cached && options.local {
        return Err(CrabError::Configuration {
            key: "lfs locks".to_owned(),
            origin: "choose only one of --cached or --local".to_owned(),
        });
    }
    if options.verify && options.local {
        return Err(CrabError::Configuration {
            key: "lfs locks --verify".to_owned(),
            origin: "cannot verify remote lock integrity from --local cache".to_owned(),
        });
    }
    if options.verify && options.cached {
        return Err(CrabError::Configuration {
            key: "lfs locks --verify".to_owned(),
            origin: "cannot verify remote lock integrity from --cached records".to_owned(),
        });
    }
    if options.id.is_some() && options.path.is_some() {
        return Err(CrabError::Configuration {
            key: "lfs locks".to_owned(),
            origin: "choose only one of --id or --path".to_owned(),
        });
    }
    Ok(())
}

fn filter_locks(mut locks: Vec<LockRecord>, options: &LfsLocksOptions) -> Vec<LockRecord> {
    if let Some(id) = &options.id {
        locks.retain(|lock| lock.id == *id);
    }
    if let Some(path) = &options.path {
        locks.retain(|lock| lock.path == *path);
    }
    if let Some(limit) = options.limit.filter(|limit| *limit > 0) {
        locks.truncate(limit);
    }
    locks
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct VerifiedLockRecord {
    path: String,
    owner: String,
    locked_at: u64,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    ours: bool,
    local: bool,
    broken: bool,
}

fn verified_lock_records(
    remote_locks: &[LockRecord],
    local_locks: &[LockRecord],
    owner: &str,
) -> Vec<VerifiedLockRecord> {
    let mut records = Vec::new();

    for lock in remote_locks {
        records.push(VerifiedLockRecord {
            path: lock.path.clone(),
            owner: lock.owner.clone(),
            locked_at: lock.locked_at,
            id: lock.id.clone(),
            expires_at: lock.expires_at,
            ours: !owner.is_empty() && lock.owner == owner,
            local: local_locks
                .iter()
                .any(|local| local.id == lock.id || local.path == lock.path),
            broken: false,
        });
    }

    for local in local_locks {
        let still_remote = remote_locks
            .iter()
            .any(|remote| remote.id == local.id || remote.path == local.path);
        if still_remote {
            continue;
        }
        records.push(VerifiedLockRecord {
            path: local.path.clone(),
            owner: local.owner.clone(),
            locked_at: local.locked_at,
            id: local.id.clone(),
            expires_at: local.expires_at,
            ours: !owner.is_empty() && local.owner == owner,
            local: true,
            broken: true,
        });
    }

    records
}

fn filter_verified_locks(
    mut records: Vec<VerifiedLockRecord>,
    options: &LfsLocksOptions,
) -> Vec<VerifiedLockRecord> {
    if let Some(id) = &options.id {
        records.retain(|lock| lock.id == *id);
    }
    if let Some(path) = &options.path {
        records.retain(|lock| lock.path == *path);
    }
    if let Some(limit) = options.limit.filter(|limit| *limit > 0) {
        records.truncate(limit);
    }
    records
}

fn emit_verified_locks(records: &[VerifiedLockRecord], mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("lfs.locks.verify", "1.1", records);
        return;
    }

    if records.is_empty() {
        eprintln!("no locks found");
        return;
    }

    for lock in records {
        let marker = if lock.broken {
            "X"
        } else if lock.ours {
            "O"
        } else {
            " "
        };
        let ts = chrono_format(lock.locked_at);
        let exp = lock
            .expires_at
            .map(|e| format!(" expires_at:{e}"))
            .unwrap_or_default();
        let broken = if lock.broken { " broken" } else { "" };
        println!(
            "{marker} {}\t{}\tID:{}\t{ts}{exp}{broken}",
            lock.path, lock.owner, lock.id
        );
    }
}

fn emit_locks(locks: &[LockRecord], mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("lfs.locks", "1.1", locks);
        return;
    }

    if locks.is_empty() {
        eprintln!("no locks found");
        return;
    }

    for lock in locks {
        let ts = chrono_format(lock.locked_at);
        let exp = lock
            .expires_at
            .map(|e| format!(" expires_at:{e}"))
            .unwrap_or_default();
        println!("{}\t{}\tID:{}\t{ts}{exp}", lock.path, lock.owner, lock.id);
    }
}

#[derive(Debug, Clone)]
struct LockCache {
    local_file: PathBuf,
    remote_file: PathBuf,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct LockCachePayload {
    locks: Vec<LockRecord>,
}

impl LockCache {
    fn open(local_lfs_dir: &Path, remote: Option<&str>) -> Result<Self> {
        let scope = cache_scope(remote);
        Ok(Self {
            local_file: local_lfs_dir.join("crab-lockcache.json"),
            remote_file: local_lfs_dir
                .join("cache")
                .join("crab-locks")
                .join(scope)
                .join("remote.json"),
        })
    }

    fn read_local(&self) -> Result<Vec<LockRecord>> {
        read_lock_cache_file(&self.local_file, true)
    }

    fn add_local(&self, record: &LockRecord) -> Result<()> {
        let _guard = lock_cache_file(&self.local_file)?;
        let mut locks = self.read_local()?;
        locks.retain(|lock| lock.path != record.path && lock.id != record.id);
        locks.push(record.clone());
        write_lock_cache_file(&self.local_file, &locks)
    }

    fn remove_local_by_path(&self, path: &str) -> Result<()> {
        let _guard = lock_cache_file(&self.local_file)?;
        let mut locks = self.read_local()?;
        locks.retain(|lock| lock.path != path);
        write_lock_cache_file(&self.local_file, &locks)
    }

    fn remove_local_by_id(&self, id: &str) -> Result<()> {
        let _guard = lock_cache_file(&self.local_file)?;
        let mut locks = self.read_local()?;
        locks.retain(|lock| lock.id != id);
        write_lock_cache_file(&self.local_file, &locks)
    }

    fn read_remote(&self) -> Result<Vec<LockRecord>> {
        read_lock_cache_file(&self.remote_file, false)
    }

    fn write_remote(&self, locks: &[LockRecord]) -> Result<()> {
        write_lock_cache_file(&self.remote_file, locks)
    }

    fn replace_local_owned(&self, remote_locks: &[LockRecord], owner: &str) -> Result<()> {
        let _guard = lock_cache_file(&self.local_file)?;
        let owned: Vec<LockRecord> = remote_locks
            .iter()
            .filter(|lock| !owner.is_empty() && lock.owner == owner)
            .cloned()
            .collect();
        write_lock_cache_file(&self.local_file, &owned)
    }
}

fn read_lock_cache_file(path: &Path, missing_is_empty: bool) -> Result<Vec<LockRecord>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<LockCachePayload>(&bytes)
            .map(|payload| payload.locks)
            .map_err(|e| CrabError::Configuration {
                key: format!("invalid LFS lock cache {}", path.display()),
                origin: e.to_string(),
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && missing_is_empty => Ok(Vec::new()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CrabError::NotFound {
            path: "cached LFS locks".to_owned(),
        }),
        Err(e) => Err(CrabError::Io(e)),
    }
}

fn write_lock_cache_file(path: &Path, locks: &[LockRecord]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    let payload = LockCachePayload {
        locks: locks.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&payload).map_err(|e| {
        CrabError::Internal(format!(
            "failed to serialize LFS lock cache {}: {e}",
            path.display()
        ))
    })?;
    let parent = path.parent().ok_or_else(|| {
        CrabError::Internal(format!("lock cache path has no parent: {}", path.display()))
    })?;
    let mut temp = tempfile::Builder::new()
        .prefix(".crab-lockcache-")
        .tempfile_in(parent)
        .map_err(CrabError::Io)?;
    temp.write_all(&bytes).map_err(CrabError::Io)?;
    temp.as_file().sync_all().map_err(CrabError::Io)?;
    temp.persist(path)
        .map(|_| ())
        .map_err(|error| CrabError::Io(error.error))
}

fn lock_cache_file(path: &Path) -> Result<File> {
    let parent = path.parent().ok_or_else(|| {
        CrabError::Internal(format!("lock cache path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            CrabError::Internal(format!(
                "lock cache path is not valid UTF-8: {}",
                path.display()
            ))
        })?;
    let lock_path = path.with_file_name(format!(".{file_name}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(CrabError::Io)?;
    LockFileExt::lock_exclusive(&file).map_err(CrabError::Io)?;
    Ok(file)
}

fn cache_scope(remote: Option<&str>) -> String {
    let raw = remote.filter(|name| !name.is_empty()).unwrap_or("default");
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn resolve_local_lfs_dir() -> Result<PathBuf> {
    let repo_root = crate::git::discover::resolve_current_worktree_root()
        .map_or_else(std::env::current_dir, Ok)?;
    let config = crate::lfs::config::LfsConfig::resolve(&repo_root)?;
    let git_dir = crate::git::discover::discover_git_dir()?;
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        repo_root.join(git_dir)
    };
    let common_git_dir = crate::git::discover::resolve_common_dir(&git_dir);
    Ok(config.storage_dir(&common_git_dir))
}

/// Parse a human-readable duration string like "24h", "7d", "30m".
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(CrabError::Internal("empty duration string".into()));
    }

    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: u64 = num_str
        .parse()
        .map_err(|_| CrabError::Internal(format!("invalid duration number: {s}")))?;

    match unit {
        "s" => Ok(Duration::from_secs(num)),
        "m" => Ok(Duration::from_secs(num * 60)),
        "h" => Ok(Duration::from_secs(num * 3600)),
        "d" => Ok(Duration::from_secs(num * 86400)),
        _ => Err(CrabError::Internal(format!(
            "unknown duration unit '{unit}'; use s, m, h, or d"
        ))),
    }
}

/// Format a Unix timestamp as a human-readable string.
fn chrono_format(unix_secs: u64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::from_secs(unix_secs);
    // Simple ISO-ish format without pulling in chrono.
    let elapsed = dt
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let days = elapsed / 86400;
    let hours = (elapsed % 86400) / 3600;
    let mins = (elapsed % 3600) / 60;
    // Approximate date from epoch days (good enough for display).
    format!("{days}d+{hours:02}:{mins:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use std::sync::Arc;

    fn memory_lock_manager() -> LockManager {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = crate::storage::Store::new(inner);
        LockManager::lfs(store, "repo")
    }

    fn lock_record(path: &str, id: &str) -> LockRecord {
        lock_record_owned(path, id, "user@example.invalid")
    }

    fn lock_record_owned(path: &str, id: &str, owner: &str) -> LockRecord {
        LockRecord {
            path: path.to_owned(),
            owner: owner.to_owned(),
            locked_at: 1,
            id: id.to_owned(),
            expires_at: None,
            released_at: None,
        }
    }

    fn list_options() -> LfsLocksOptions {
        LfsLocksOptions {
            remote: None,
            id: None,
            path: None,
            local: false,
            cached: false,
            mode: OutputMode::Text,
            verify: false,
            limit: None,
        }
    }

    #[test]
    fn filter_locks_filters_by_id() {
        let mut options = list_options();
        options.id = Some("b".to_owned());
        let locks = vec![lock_record("a.bin", "a"), lock_record("b.bin", "b")];

        let filtered = filter_locks(locks, &options);
        assert_eq!(filtered, vec![lock_record("b.bin", "b")]);
    }

    #[test]
    fn filter_locks_filters_by_path_and_limit() {
        let mut options = list_options();
        options.path = Some("a.bin".to_owned());
        options.limit = Some(1);
        let locks = vec![
            lock_record("a.bin", "a"),
            lock_record("a.bin", "b"),
            lock_record("b.bin", "c"),
        ];

        let filtered = filter_locks(locks, &options);
        assert_eq!(filtered, vec![lock_record("a.bin", "a")]);
    }

    #[test]
    fn locks_options_reject_invalid_cache_mode_combinations() {
        let mut options = list_options();
        options.cached = true;
        options.limit = Some(1);
        assert!(validate_locks_options(&options).is_err());

        let mut options = list_options();
        options.cached = true;
        options.id = Some("abc".to_owned());
        assert!(validate_locks_options(&options).is_err());

        let mut options = list_options();
        options.cached = true;
        options.local = true;
        assert!(validate_locks_options(&options).is_err());

        let mut options = list_options();
        options.verify = true;
        options.local = true;
        assert!(validate_locks_options(&options).is_err());
    }

    #[test]
    fn locks_options_accept_verify_with_filters_and_limit() {
        let mut options = list_options();
        options.verify = true;
        options.id = Some("abc".to_owned());
        assert!(validate_locks_options(&options).is_ok());

        let mut options = list_options();
        options.verify = true;
        options.path = Some("a.bin".to_owned());
        options.limit = Some(1);
        assert!(validate_locks_options(&options).is_ok());
    }

    #[test]
    fn locks_options_accept_local_and_cached_modes() {
        let mut options = list_options();
        options.local = true;
        assert!(validate_locks_options(&options).is_ok());

        let mut options = list_options();
        options.cached = true;
        assert!(validate_locks_options(&options).is_ok());
    }

    #[test]
    fn lock_cache_adds_filters_and_removes_local_records() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LockCache::open(dir.path(), Some("origin")).unwrap();

        cache.add_local(&lock_record("a.bin", "a")).unwrap();
        cache.add_local(&lock_record("b.bin", "b")).unwrap();
        cache.add_local(&lock_record("a.bin", "a2")).unwrap();

        let locks = cache.read_local().unwrap();
        assert_eq!(
            locks,
            vec![lock_record("b.bin", "b"), lock_record("a.bin", "a2")]
        );

        cache.remove_local_by_id("b").unwrap();
        assert_eq!(
            cache.read_local().unwrap(),
            vec![lock_record("a.bin", "a2")]
        );

        cache.remove_local_by_path("a.bin").unwrap();
        assert!(cache.read_local().unwrap().is_empty());
    }

    #[test]
    fn lock_cache_remote_cache_errors_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LockCache::open(dir.path(), None).unwrap();

        let err = cache.read_remote().unwrap_err();

        assert!(matches!(err, CrabError::NotFound { .. }));
    }

    #[test]
    fn lock_cache_round_trips_remote_records_by_scope() {
        let dir = tempfile::tempdir().unwrap();
        let origin = LockCache::open(dir.path(), Some("origin")).unwrap();
        let upstream = LockCache::open(dir.path(), Some("upstream")).unwrap();

        origin
            .write_remote(&[lock_record("a.bin", "a"), lock_record("b.bin", "b")])
            .unwrap();

        assert_eq!(
            origin.read_remote().unwrap(),
            vec![lock_record("a.bin", "a"), lock_record("b.bin", "b")]
        );
        assert!(matches!(
            upstream.read_remote().unwrap_err(),
            CrabError::NotFound { .. }
        ));
    }

    #[test]
    fn filter_locks_ignores_zero_limit() {
        let mut options = list_options();
        options.limit = Some(0);
        let locks = vec![lock_record("a.bin", "a"), lock_record("b.bin", "b")];

        let filtered = filter_locks(locks.clone(), &options);

        assert_eq!(filtered, locks);
    }

    #[test]
    fn verified_lock_records_mark_owned_and_broken_locks() {
        let remote = vec![
            lock_record_owned("owned.bin", "owned", "user@example.invalid"),
            lock_record_owned("other.bin", "other", "other@example.invalid"),
        ];
        let local = vec![
            lock_record_owned("owned.bin", "owned", "user@example.invalid"),
            lock_record_owned("gone.bin", "gone", "user@example.invalid"),
        ];

        let records = verified_lock_records(&remote, &local, "user@example.invalid");

        assert_eq!(records.len(), 3);
        assert!(records.iter().any(|record| {
            record.path == "owned.bin" && record.ours && record.local && !record.broken
        }));
        assert!(records.iter().any(|record| {
            record.path == "other.bin" && !record.ours && !record.local && !record.broken
        }));
        assert!(records.iter().any(|record| {
            record.path == "gone.bin" && record.ours && record.local && record.broken
        }));
    }

    #[test]
    fn filter_verified_locks_applies_id_path_and_limit() {
        let remote = vec![
            lock_record_owned("a.bin", "a1", "user@example.invalid"),
            lock_record_owned("a.bin", "a2", "other@example.invalid"),
            lock_record_owned("b.bin", "b1", "other@example.invalid"),
        ];
        let records = verified_lock_records(&remote, &[], "user@example.invalid");

        let mut by_id = list_options();
        by_id.id = Some("a2".to_owned());
        assert_eq!(filter_verified_locks(records.clone(), &by_id).len(), 1);
        assert_eq!(filter_verified_locks(records.clone(), &by_id)[0].id, "a2");

        let mut by_path = list_options();
        by_path.path = Some("a.bin".to_owned());
        by_path.limit = Some(1);
        let filtered = filter_verified_locks(records, &by_path);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].path, "a.bin");
    }

    #[test]
    fn lock_cache_replace_local_owned_drops_stale_and_other_owner_locks() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LockCache::open(dir.path(), Some("origin")).unwrap();
        cache
            .add_local(&lock_record_owned(
                "stale.bin",
                "stale",
                "user@example.invalid",
            ))
            .unwrap();

        let remote = vec![
            lock_record_owned("owned.bin", "owned", "user@example.invalid"),
            lock_record_owned("other.bin", "other", "other@example.invalid"),
        ];
        cache
            .replace_local_owned(&remote, "user@example.invalid")
            .unwrap();

        assert_eq!(
            cache.read_local().unwrap(),
            vec![lock_record_owned(
                "owned.bin",
                "owned",
                "user@example.invalid"
            )]
        );
    }

    #[test]
    fn unlock_options_accept_id_without_path() {
        let options = LfsUnlockOptions {
            id: Some("abc".to_owned()),
            ..LfsUnlockOptions::default()
        };

        assert!(validate_unlock_options(&options).is_ok());
    }

    #[test]
    fn unlock_options_reject_missing_or_ambiguous_target() {
        assert!(validate_unlock_options(&LfsUnlockOptions::default()).is_err());

        let options = LfsUnlockOptions {
            path: Some("a.bin".to_owned()),
            id: Some("abc".to_owned()),
            ..LfsUnlockOptions::default()
        };
        assert!(validate_unlock_options(&options).is_err());
    }

    #[test]
    fn validate_worktree_file_accepts_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset.bin");
        std::fs::write(&path, b"content").unwrap();

        assert!(validate_worktree_file(path.to_str().unwrap(), "lfs lock").is_ok());
    }

    #[test]
    fn validate_worktree_file_rejects_missing_or_directory_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.bin");
        assert!(validate_worktree_file(missing.to_str().unwrap(), "lfs lock").is_err());

        assert!(validate_worktree_file(dir.path().to_str().unwrap(), "lfs lock").is_err());
    }

    #[tokio::test]
    async fn unlock_target_resolves_id_to_path() {
        let mgr = memory_lock_manager();
        let record = mgr.lock("a.bin", "user@example.invalid").await.unwrap();
        let options = LfsUnlockOptions {
            id: Some(record.id.clone()),
            ..LfsUnlockOptions::default()
        };

        let target = unlock_target(&mgr, &options).await.unwrap();

        assert_eq!(target, ("a.bin".to_owned(), Some(record.id)));
    }

    #[tokio::test]
    async fn lock_by_id_rejects_missing_id() {
        let mgr = memory_lock_manager();

        let err = lock_by_id(&mgr, "missing").await.unwrap_err();

        assert!(matches!(err, CrabError::NotFound { .. }));
    }
}
