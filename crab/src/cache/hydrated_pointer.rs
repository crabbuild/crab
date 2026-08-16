//! Per-worktree cache mapping working-tree paths to the pointer bytes that
//! produced their current content.
//!
//! After `crab hydrate` reconstructs a file, the working-tree copy
//! is full content (not a pointer) — but git has a pointer blob in
//! the index, so `git status` / `git diff` / `git pull` all invoke
//! the clean filter on the hydrated bytes to decide whether the
//! file changed. Without this cache, every such invocation would
//! re-hash and re-chunk the entire file (slow on multi-GiB files)
//! and require acquiring `LOCK_EX` on `.crab/staging` — which
//! fails as [`CrabError::StagingLocked`] whenever another crab
//! process (e.g. a shell-prompt `git status`) is mid-flight, leaving
//! the user unable to `git diff` or `git pull` hydrated files.
//!
//! The cache is keyed on a **stat fingerprint** (`mtime_ns`, `size`)
//! so concurrent modifications invalidate the entry correctly — a
//! `touch` or partial write produces a different mtime, so the clean
//! filter falls through to the normal CDC pipeline.
//!
//! Concurrency: writes use tempfile + atomic rename. Concurrent
//! hydrators may overwrite each other's additions, but the last
//! writer wins with a consistent file. A corrupt or missing cache
//! degrades gracefully to the slow path — nothing in the protocol
//! depends on it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::core::error::{CrabError, Result};

/// Filename inside `.crab/` holding the hydrated-pointer cache.
pub const HYDRATED_POINTERS_FILENAME: &str = "hydrated-pointers.json";

/// Stat fingerprint plus the pointer blob that a hydrated working-tree
/// file reconstructs back into. Used by the clean filter to skip
/// CDC/hashing/staging for files whose content matches a known pointer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HydratedEntry {
    /// File modification time in nanoseconds since the UNIX epoch.
    pub mtime_ns: i128,
    /// File size in bytes, as observed by `fs::metadata` after hydrate.
    pub size: u64,
    /// Hex-encoded pointer blob bytes. Stored hex-encoded to keep the
    /// JSON file human-inspectable and avoid base64 quirks.
    pub pointer_hex: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OnDiskFile {
    /// Map from repo-relative path (forward-slashes) to entry.
    #[serde(default)]
    entries: HashMap<String, HydratedEntry>,
}

/// In-memory cache. Cheap to clone (map of small strings) and safe to
/// mutate from a single thread — concurrent writers arbitrate at the
/// filesystem level via atomic rename.
#[derive(Debug, Clone, Default)]
pub struct HydratedPointerCache {
    entries: HashMap<String, HydratedEntry>,
}

impl HydratedPointerCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the cache from `path`, synchronously.
    ///
    /// Returns an empty cache if the file is missing or corrupt. A
    /// corrupt cache is logged at `warn!` and treated as empty so a
    /// bad file can never block the clean filter.
    #[must_use]
    pub fn load_sync(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    path = %path.display(),
                    "hydrated-pointer cache missing, starting empty"
                );
                return Self::new();
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "hydrated-pointer cache unreadable, treating as empty"
                );
                return Self::new();
            }
        };
        match serde_json::from_slice::<OnDiskFile>(&bytes) {
            Ok(parsed) => Self {
                entries: parsed.entries,
            },
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "hydrated-pointer cache corrupt, treating as empty"
                );
                Self::new()
            }
        }
    }

    /// Number of entries currently in the cache.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache has any entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up the entry for `rel_path`, if any. The caller is
    /// responsible for validating the stat fingerprint via
    /// [`matches_stat`](Self::matches_stat).
    #[must_use]
    pub fn get(&self, rel_path: &str) -> Option<&HydratedEntry> {
        self.entries.get(rel_path)
    }

    /// Iterate over advisory entries without exposing mutation.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&str, &HydratedEntry)> {
        self.entries
            .iter()
            .map(|(path, entry)| (path.as_str(), entry))
    }

    /// Insert (or overwrite) an entry. Overwrites are common: every
    /// successful `crab hydrate` refreshes the fingerprint.
    pub fn insert(&mut self, rel_path: String, entry: HydratedEntry) {
        self.entries.insert(rel_path, entry);
    }

    /// Remove an entry. Returns `true` if a row was removed. Used when
    /// the caller observes a stale entry (stat mismatch) and wants to
    /// avoid re-checking it on every subsequent clean.
    pub fn remove(&mut self, rel_path: &str) -> bool {
        self.entries.remove(rel_path).is_some()
    }

    /// Atomically save the cache to `path` via tempfile + rename.
    ///
    /// Creates the parent directory if needed. A failure to save is
    /// surfaced so callers can log it; writers treat save errors as
    /// best-effort (the cache will simply be re-populated on the next
    /// hydrate).
    pub fn save_sync(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }

        let on_disk = OnDiskFile {
            entries: self.entries.clone(),
        };
        let body = serde_json::to_vec(&on_disk).map_err(|e| {
            CrabError::Internal(format!("failed to serialize hydrated-pointer cache: {e}"))
        })?;

        let tmp = tmp_path(path);
        std::fs::write(&tmp, &body).map_err(CrabError::Io)?;
        std::fs::rename(&tmp, path).map_err(CrabError::Io)?;
        debug!(
            path = %path.display(),
            entries = self.entries.len(),
            "saved hydrated-pointer cache"
        );
        Ok(())
    }

    /// Merge `updates` into the on-disk cache at `path` and rewrite
    /// atomically. Load-modify-save so a concurrent writer's entries
    /// for unrelated keys are preserved (last writer wins per key).
    pub fn update_on_disk<I>(path: &Path, updates: I) -> Result<()>
    where
        I: IntoIterator<Item = (String, HydratedEntry)>,
    {
        let mut cache = Self::load_sync(path);
        for (k, v) in updates {
            cache.insert(k, v);
        }
        cache.save_sync(path)
    }

    /// Remove entries for each path in `paths` and rewrite the cache
    /// atomically. Used when the caller invalidates stale entries
    /// (stat mismatch during clean) so the next session doesn't re-do
    /// the lookup only to fall through again.
    pub fn invalidate_on_disk<I>(path: &Path, paths: I) -> Result<()>
    where
        I: IntoIterator<Item = String>,
    {
        let mut cache = Self::load_sync(path);
        let mut touched = false;
        for p in paths {
            touched |= cache.remove(&p);
        }
        if touched {
            cache.save_sync(path)
        } else {
            Ok(())
        }
    }
}

/// Build the canonical per-worktree cache path for a resolved context.
#[must_use]
pub fn cache_path_for_context(ctx: &crate::git::worktree::WorktreeContext) -> PathBuf {
    ctx.per_worktree_crab_dir.join(HYDRATED_POINTERS_FILENAME)
}

/// Build the canonical per-worktree cache path for `worktree_root`.
pub fn cache_path_for_worktree_root(worktree_root: &Path) -> Result<PathBuf> {
    let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(worktree_root)?;
    Ok(cache_path_for_context(&ctx))
}

/// Construct a stat-fingerprinted [`HydratedEntry`] for `path`.
///
/// Reads metadata once; returns an IO error if the file is missing or
/// its metadata cannot be read. `mtime_ns` may saturate to `i128::MIN`
/// for timestamps before the UNIX epoch, which is fine — those are
/// never observed on hydrated files in practice.
pub fn entry_for_path(path: &Path, pointer_bytes: &[u8]) -> Result<HydratedEntry> {
    let meta = std::fs::metadata(path).map_err(CrabError::Io)?;
    let size = meta.len();
    let mtime_ns = systemtime_to_nanos(meta.modified().ok());
    Ok(HydratedEntry {
        mtime_ns,
        size,
        pointer_hex: hex_encode(pointer_bytes),
    })
}

/// Compare a cached entry's fingerprint against the current stat of
/// `path`. Returns `true` when `mtime_ns` and `size` match exactly.
/// A false result means the file has been modified since hydrate and
/// the caller must fall back to the normal clean pipeline.
#[must_use]
pub fn matches_stat(path: &Path, entry: &HydratedEntry) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() != entry.size {
        return false;
    }
    systemtime_to_nanos(meta.modified().ok()) == entry.mtime_ns
}

/// Decode the hex-encoded pointer bytes from a cache entry.
///
/// A corrupt hex string returns `None`; callers treat that as a cache
/// miss and fall through to the CDC pipeline.
#[must_use]
pub fn decode_pointer(entry: &HydratedEntry) -> Option<Vec<u8>> {
    hex_decode(&entry.pointer_hex)
}

/// Encode bytes as a lowercase ASCII hex string.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Decode a lowercase/uppercase ASCII hex string. Returns `None` on
/// odd length or non-hex characters.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn systemtime_to_nanos(t: Option<SystemTime>) -> i128 {
    // Duration since UNIX_EPOCH; negate if the time is before epoch
    // (exceptionally rare on hydrated files, but harmless to handle).
    match t {
        Some(st) => match st.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => i128::try_from(d.as_nanos()).unwrap_or(i128::MAX),
            Err(e) => {
                let before = e.duration();
                -i128::try_from(before.as_nanos()).unwrap_or(i128::MAX)
            }
        },
        None => 0,
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_file(path: &Path, bytes: &[u8]) {
        let mut f = std::fs::File::create(path).expect("create");
        f.write_all(bytes).expect("write");
        f.sync_all().expect("sync");
    }

    #[test]
    fn load_missing_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let cache = HydratedPointerCache::load_sync(&path);
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn load_corrupt_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        write_file(&path, b"{not valid json");
        let cache = HydratedPointerCache::load_sync(&path);
        assert!(cache.is_empty());
    }

    #[test]
    fn insert_save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let mut cache = HydratedPointerCache::new();
        cache.insert(
            "a.zip".to_owned(),
            HydratedEntry {
                mtime_ns: 123,
                size: 456,
                pointer_hex: "deadbeef".to_owned(),
            },
        );
        cache.save_sync(&path).expect("save");

        let loaded = HydratedPointerCache::load_sync(&path);
        let e = loaded.get("a.zip").expect("entry");
        assert_eq!(e.mtime_ns, 123);
        assert_eq!(e.size, 456);
        assert_eq!(e.pointer_hex, "deadbeef");
        assert_eq!(
            loaded.entries().map(|(path, _)| path).collect::<Vec<_>>(),
            ["a.zip"]
        );
    }

    #[test]
    fn update_on_disk_preserves_other_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let mut cache = HydratedPointerCache::new();
        cache.insert(
            "keep.bin".to_owned(),
            HydratedEntry {
                mtime_ns: 1,
                size: 1,
                pointer_hex: "00".to_owned(),
            },
        );
        cache.save_sync(&path).expect("save");

        HydratedPointerCache::update_on_disk(
            &path,
            [(
                "new.bin".to_owned(),
                HydratedEntry {
                    mtime_ns: 2,
                    size: 2,
                    pointer_hex: "11".to_owned(),
                },
            )],
        )
        .expect("update");

        let loaded = HydratedPointerCache::load_sync(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.get("keep.bin").is_some());
        assert!(loaded.get("new.bin").is_some());
    }

    #[test]
    fn entry_for_path_reads_metadata() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("f.bin");
        write_file(&p, b"hello world");
        let entry = entry_for_path(&p, b"pointer-bytes").expect("entry");
        assert_eq!(entry.size, 11);
        assert!(!entry.pointer_hex.is_empty());
        assert!(matches_stat(&p, &entry));
    }

    #[test]
    fn matches_stat_detects_size_change() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("f.bin");
        write_file(&p, b"hello");
        let entry = entry_for_path(&p, b"ptr").expect("entry");
        write_file(&p, b"hello world"); // size grew
        assert!(!matches_stat(&p, &entry));
    }

    #[test]
    fn matches_stat_returns_false_for_missing_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("missing.bin");
        let entry = HydratedEntry {
            mtime_ns: 0,
            size: 0,
            pointer_hex: String::new(),
        };
        assert!(!matches_stat(&p, &entry));
    }

    #[test]
    fn decode_pointer_round_trip() {
        let entry = HydratedEntry {
            mtime_ns: 0,
            size: 0,
            pointer_hex: hex_encode(b"pointer"),
        };
        assert_eq!(decode_pointer(&entry).as_deref(), Some(&b"pointer"[..]));
    }

    #[test]
    fn decode_pointer_rejects_invalid_hex() {
        let entry = HydratedEntry {
            mtime_ns: 0,
            size: 0,
            pointer_hex: "zz".to_owned(),
        };
        assert!(decode_pointer(&entry).is_none());
    }

    #[test]
    fn invalidate_on_disk_removes_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let mut cache = HydratedPointerCache::new();
        cache.insert(
            "a".to_owned(),
            HydratedEntry {
                mtime_ns: 1,
                size: 1,
                pointer_hex: "00".to_owned(),
            },
        );
        cache.insert(
            "b".to_owned(),
            HydratedEntry {
                mtime_ns: 2,
                size: 2,
                pointer_hex: "11".to_owned(),
            },
        );
        cache.save_sync(&path).expect("save");

        HydratedPointerCache::invalidate_on_disk(&path, ["a".to_owned()]).expect("invalidate");

        let loaded = HydratedPointerCache::load_sync(&path);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get("a").is_none());
        assert!(loaded.get("b").is_some());
    }
}
