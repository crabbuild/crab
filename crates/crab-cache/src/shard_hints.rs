//! Persistent `file_hash → shard_hash` mapping used by the clean filter
//! to populate `shard-hint` fields in pointer blobs.
//!
//! The cache is a simple JSON file at `{cache_root}/shard-hints.json`
//! that the push pipeline writes after step 8 (shard building) and the
//! clean filter reads on each `git add`. Hints are advisory — a stale
//! or missing entry causes the reader to fall back to the file-index
//! lookup path, so corruption or concurrent-write races are recoverable.
//!
//! Concurrency: writes use tempfile + atomic rename. Concurrent pushes
//! may overwrite each other's additions, but the last writer wins with
//! a consistent file. Readers that observe a partial write see either
//! the previous file (pre-rename) or the new file (post-rename), never
//! a mix.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use crate::{CacheError, Result};
use crab_types::pointer::Pointer;
use crab_xet::xorb::format::MerkleHash;

/// Filename for the shard-hint JSON cache inside the crab cache root.
pub const SHARD_HINTS_FILENAME: &str = "shard-hints.json";
/// Maximum serialized shard-hint cache body read into memory.
pub const MAX_SHARD_HINTS_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SHARD_HINTS_ENTRIES: usize = 1_000_000;

/// On-disk representation: hex-encoded file_hash → hex-encoded shard_hash.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ShardHintsFile {
    /// Map from file_hash hex to shard_hash hex.
    hints: HashMap<String, String>,
}

/// In-memory shard-hint mapping.
///
/// Keys and values are kept as `MerkleHash` to avoid repeated hex
/// conversion at lookup time. Serialization to JSON uses hex strings
/// for human-readability and compatibility with the existing
/// `MerkleHash::hex()` / `MerkleHash::from_hex()` helpers.
#[derive(Debug, Clone, Default)]
pub struct ShardHintCache {
    hints: HashMap<MerkleHash, MerkleHash>,
}

impl ShardHintCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the cache from `path`.
    ///
    /// Returns an empty cache if the file does not exist. A malformed
    /// JSON file is treated as empty (with a `warn!` log) rather than
    /// failing — hints are advisory, so a corrupt cache degrades to the
    /// slow path instead of breaking push.
    pub async fn load(path: &Path) -> Result<Self> {
        let bytes = match read_async_bounded(path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "shard-hints cache missing, starting empty");
                return Ok(Self::new());
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "shard-hints cache exceeds the safety limit, treating as empty"
                );
                return Ok(Self::new());
            }
            Err(e) => return Err(CacheError::Io(e)),
        };
        Ok(Self::from_bytes(path, &bytes))
    }

    /// Synchronous variant of [`load`](Self::load).
    ///
    /// Uses blocking `std::fs` so it can be called from non-async contexts
    /// like the git filter-process clean loop (which runs inside
    /// `spawn_blocking`) without needing a tokio handle.
    pub fn load_sync(path: &Path) -> Result<Self> {
        let bytes = match read_sync_bounded(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "shard-hints cache missing, starting empty");
                return Ok(Self::new());
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "shard-hints cache exceeds the safety limit, treating as empty"
                );
                return Ok(Self::new());
            }
            Err(e) => return Err(CacheError::Io(e)),
        };
        Ok(Self::from_bytes(path, &bytes))
    }

    /// Parse a shard-hints JSON blob, tolerating corrupt data by returning
    /// an empty cache with a `warn!` log — hints are advisory.
    fn from_bytes(path: &Path, bytes: &[u8]) -> Self {
        if bytes.len() as u64 > MAX_SHARD_HINTS_BYTES {
            warn!(
                path = %path.display(),
                bytes = bytes.len(),
                limit = MAX_SHARD_HINTS_BYTES,
                "shard-hints cache exceeds the safety limit, treating as empty"
            );
            return Self::new();
        }
        let parsed: ShardHintsFile = match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "shard-hints cache corrupt, treating as empty"
                );
                return Self::new();
            }
        };
        if parsed.hints.len() > MAX_SHARD_HINTS_ENTRIES {
            warn!(
                path = %path.display(),
                entries = parsed.hints.len(),
                limit = MAX_SHARD_HINTS_ENTRIES,
                "shard-hints cache contains too many entries, treating as empty"
            );
            return Self::new();
        }

        let mut hints = HashMap::with_capacity(parsed.hints.len());
        for (file_hex, shard_hex) in parsed.hints {
            let Ok(file_hash) = MerkleHash::from_hex(&file_hex) else {
                warn!(file_hex = %file_hex, "invalid file_hash in shard-hints cache, skipping");
                continue;
            };
            let Ok(shard_hash) = MerkleHash::from_hex(&shard_hex) else {
                warn!(shard_hex = %shard_hex, "invalid shard_hash in shard-hints cache, skipping");
                continue;
            };
            hints.insert(file_hash, shard_hash);
        }

        debug!(path = %path.display(), entries = hints.len(), "loaded shard-hints cache");
        Self { hints }
    }

    /// Look up the shard hash associated with `file_hash`, if any.
    #[must_use]
    pub fn get(&self, file_hash: &MerkleHash) -> Option<MerkleHash> {
        self.hints.get(file_hash).copied()
    }

    /// Build a crab pointer for `file_hash`, attaching a cached shard
    /// hint when one is known.
    #[must_use]
    pub fn pointer_for(&self, file_hash: [u8; 32], size: u64) -> Pointer {
        let key = MerkleHash::from(file_hash);
        let shard_hint = self.get(&key).map(<[u8; 32]>::from);
        Pointer {
            file_hash,
            size,
            shard_hint,
        }
    }

    /// Number of entries in the cache.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hints.len()
    }

    /// Whether the cache has any entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hints.is_empty()
    }

    /// Insert a single mapping, overwriting any existing entry for
    /// `file_hash`.
    pub fn insert(&mut self, file_hash: MerkleHash, shard_hash: MerkleHash) {
        self.hints.insert(file_hash, shard_hash);
    }

    /// Merge all `(file_hash, shard_hash)` pairs from `entries` into
    /// the cache. Existing entries are overwritten.
    pub fn insert_all<I>(&mut self, entries: I)
    where
        I: IntoIterator<Item = (MerkleHash, MerkleHash)>,
    {
        self.hints.extend(entries);
    }

    /// Atomically write the cache to `path`.
    ///
    /// Uses tempfile + rename so a concurrent reader always observes
    /// either the old file or the fully-written new file.
    pub async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let on_disk = ShardHintsFile {
            hints: self.hints.iter().map(|(f, s)| (f.hex(), s.hex())).collect(),
        };
        if on_disk.hints.len() > MAX_SHARD_HINTS_ENTRIES {
            return Err(CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: format!(
                    "shard-hints cache contains {} entries; limit is {MAX_SHARD_HINTS_ENTRIES}",
                    on_disk.hints.len()
                ),
            });
        }
        let body = serde_json::to_vec(&on_disk).map_err(|e| {
            CacheError::Internal(format!("failed to serialize shard-hints cache: {e}"))
        })?;
        if body.len() as u64 > MAX_SHARD_HINTS_BYTES {
            return Err(CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: format!(
                    "serialized shard-hints cache is {} bytes; limit is {MAX_SHARD_HINTS_BYTES}",
                    body.len()
                ),
            });
        }

        use std::sync::atomic::{AtomicU64, Ordering};
        static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
        let tmp = tmp_path(
            path,
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
        );
        let mut f = tokio::fs::File::create(&tmp).await?;
        let write_result = async {
            f.write_all(&body).await?;
            f.flush().await?;
            drop(f);
            tokio::fs::rename(&tmp, path).await
        }
        .await;
        if let Err(error) = write_result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error.into());
        }

        debug!(path = %path.display(), entries = self.hints.len(), "saved shard-hints cache");
        Ok(())
    }

    /// Merge `entries` into an on-disk cache at `path` and rewrite
    /// atomically. Reads existing entries first so concurrent pushes
    /// don't drop each other's contributions (last writer wins per key,
    /// but unrelated keys are preserved).
    pub async fn update_on_disk<I>(path: &Path, entries: I) -> Result<()>
    where
        I: IntoIterator<Item = (MerkleHash, MerkleHash)>,
    {
        let mut cache = Self::load(path).await?;
        cache.insert_all(entries);
        cache.save(path).await
    }
}

async fn read_async_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    file.take(MAX_SHARD_HINTS_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_SHARD_HINTS_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {MAX_SHARD_HINTS_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

fn read_sync_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_SHARD_HINTS_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SHARD_HINTS_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {MAX_SHARD_HINTS_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

/// Default on-disk path for the shard-hint cache.
#[must_use]
pub fn default_path() -> PathBuf {
    super::default_cache_root().join(SHARD_HINTS_FILENAME)
}

fn tmp_path(path: &Path, pid: u32, sequence: u64) -> PathBuf {
    match path.file_name() {
        Some(name) => {
            let mut tmp_name = std::ffi::OsString::from(".");
            tmp_name.push(name);
            tmp_name.push(format!(".tmp.{pid}.{sequence}"));
            match path.parent() {
                Some(parent) => parent.join(tmp_name),
                None => PathBuf::from(tmp_name),
            }
        }
        None => path.with_extension("tmp"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_hash(seed: u64) -> MerkleHash {
        MerkleHash::from([
            seed,
            seed.wrapping_add(1),
            seed.wrapping_add(2),
            seed.wrapping_add(3),
        ])
    }

    #[tokio::test]
    async fn load_missing_file_returns_empty_cache() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.json");
        let cache = ShardHintCache::load(&path).await.unwrap();
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn save_then_load_roundtrips_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("shard-hints.json");

        let file_hash = make_hash(1);
        let shard_hash = make_hash(42);
        let mut cache = ShardHintCache::new();
        cache.insert(file_hash, shard_hash);

        cache.save(&path).await.unwrap();

        let loaded = ShardHintCache::load(&path).await.unwrap();
        assert_eq!(loaded.get(&file_hash), Some(shard_hash));
        assert_eq!(loaded.len(), 1);
    }

    #[tokio::test]
    async fn insert_all_merges_entries() {
        let mut cache = ShardHintCache::new();
        let f1 = make_hash(1);
        let s1 = make_hash(11);
        let f2 = make_hash(2);
        let s2 = make_hash(22);
        cache.insert_all(vec![(f1, s1), (f2, s2)]);
        assert_eq!(cache.get(&f1), Some(s1));
        assert_eq!(cache.get(&f2), Some(s2));
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test]
    async fn insert_overwrites_existing_entry() {
        let mut cache = ShardHintCache::new();
        let file_hash = make_hash(1);
        cache.insert(file_hash, make_hash(10));
        cache.insert(file_hash, make_hash(20));
        assert_eq!(cache.get(&file_hash), Some(make_hash(20)));
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_hash() {
        let cache = ShardHintCache::new();
        assert!(cache.get(&make_hash(99)).is_none());
    }

    #[tokio::test]
    async fn pointer_for_attaches_cached_shard_hint() {
        let file_hash = make_hash(1);
        let shard_hash = make_hash(2);
        let mut cache = ShardHintCache::new();
        cache.insert(file_hash, shard_hash);

        let pointer = cache.pointer_for(file_hash.into(), 123);

        assert_eq!(pointer.file_hash, <[u8; 32]>::from(file_hash));
        assert_eq!(pointer.size, 123);
        assert_eq!(pointer.shard_hint, Some(shard_hash.into()));
    }

    #[tokio::test]
    async fn corrupt_file_is_treated_as_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("shard-hints.json");
        tokio::fs::write(&path, b"{ not valid json").await.unwrap();

        let cache = ShardHintCache::load(&path).await.unwrap();
        assert!(cache.is_empty());
    }

    #[test]
    fn oversized_body_is_treated_as_empty() {
        let body = vec![b' '; usize::try_from(MAX_SHARD_HINTS_BYTES).unwrap() + 1];
        let cache = ShardHintCache::from_bytes(Path::new("oversized"), &body);
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn invalid_hex_entries_are_skipped_not_fatal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("shard-hints.json");
        let file_hash = make_hash(1);
        let shard_hash = make_hash(2);
        let body = format!(
            r#"{{"hints":{{"{}":"{}","zzz":"notahex"}}}}"#,
            file_hash.hex(),
            shard_hash.hex()
        );
        tokio::fs::write(&path, body.as_bytes()).await.unwrap();

        let cache = ShardHintCache::load(&path).await.unwrap();
        assert_eq!(cache.get(&file_hash), Some(shard_hash));
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn save_creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let path = dir
            .path()
            .join("nested")
            .join("dir")
            .join("shard-hints.json");
        let mut cache = ShardHintCache::new();
        cache.insert(make_hash(1), make_hash(2));
        cache.save(&path).await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn update_on_disk_preserves_unrelated_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("shard-hints.json");

        let f1 = make_hash(1);
        let s1 = make_hash(11);
        let mut cache = ShardHintCache::new();
        cache.insert(f1, s1);
        cache.save(&path).await.unwrap();

        let f2 = make_hash(2);
        let s2 = make_hash(22);
        ShardHintCache::update_on_disk(&path, vec![(f2, s2)])
            .await
            .unwrap();

        let loaded = ShardHintCache::load(&path).await.unwrap();
        assert_eq!(loaded.get(&f1), Some(s1));
        assert_eq!(loaded.get(&f2), Some(s2));
        assert_eq!(loaded.len(), 2);
    }
}
