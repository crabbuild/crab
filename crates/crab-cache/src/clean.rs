//! Ownership policy for explicit local payload cleanup.

use std::path::{Component, Path};

use tokio_util::sync::CancellationToken;

use crate::CacheError;
use crate::Result;

/// Cleanup totals; retained subtrees are counted once, without inspecting their contents.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct CacheCleanReport {
    pub files_removed: u64,
    pub bytes_reclaimed: u64,
    pub retained_entries: u64,
    pub busy_entries: u64,
    pub unsafe_entries: u64,
    pub dry_run: bool,
}

/// Remove recognized private cache payloads, preserving live, unknown, and busy state.
///
/// Dry runs report eligible files without changing filesystem contents. Missing
/// roots remain missing. Cancellation waits for the worker to stop deleting.
pub async fn clean_cache(
    root: &Path,
    dry_run: bool,
    cancel: &CancellationToken,
) -> Result<CacheCleanReport> {
    run_cleanup(root, dry_run, cancel, |pinned, root, dry_run, cancel| {
        let mut removal =
            crate::catalog::PayloadRemoval::open(Some(pinned), root, Path::new(""), dry_run)?;
        pinned.clean(dry_run, cancel, &mut |relative, operation| {
            removal.clean(relative, operation)
        })
    })
    .await
}

async fn run_cleanup(
    root: &Path,
    dry_run: bool,
    cancel: &CancellationToken,
    clean: fn(
        &crate::private_fs::PinnedRoot,
        &Path,
        bool,
        &CancellationToken,
    ) -> Result<CacheCleanReport>,
) -> Result<CacheCleanReport> {
    let root = root.to_owned();
    crate::private_fs::run_blocking(cancel, move |cancel| {
        let pinned = match crate::private_fs::PinnedRoot::open(&root) {
            Ok(pinned) => pinned,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CacheCleanReport {
                    dry_run,
                    ..Default::default()
                });
            }
            Err(error) => return Err(error),
        };
        // Directory owners protect mutable mirror workspaces; payload locks
        // protect individual fills/readers. Retain both through deletion, but
        // keep previews read-only and never use the recursive cleanup method.
        let _directory_owner = if dry_run {
            None
        } else {
            Some(crate::lifecycle::CacheCleanGuard::acquire(&root, cancel)?)
        };
        clean(&pinned, &root, dry_run, cancel)
    })
    .await
}

pub(crate) enum EntryKind {
    Directory,
    Payload,
    Temporary,
    Retain,
}

// Cache namespaces are not recursive deletion authority. Follow only the
// fixed payload layouts; databases, workspaces, profiles, and temporary files
// retain their own lifecycle, even when placed inside a recognized directory.
pub(crate) fn entry_kind(relative: &Path) -> EntryKind {
    let components: Option<Vec<_>> = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect();
    let Some(parts) = components else {
        return EntryKind::Retain;
    };
    if parts == ["namespaces"] {
        return EntryKind::Directory;
    }
    let parts = match parts.as_slice() {
        ["namespaces", namespace, payload @ ..] if hex(namespace, 64) => {
            if payload.is_empty() {
                return EntryKind::Directory;
            }
            payload
        }
        ["namespaces", ..] => return EntryKind::Retain,
        parts => parts,
    };
    let kind = match object_entry_kind(parts) {
        EntryKind::Retain => range_entry_kind(parts),
        kind => kind,
    };
    if matches!(kind, EntryKind::Retain)
        && parts.last().is_some_and(|name| name.starts_with(".tmp-"))
    {
        let parent = &parts[..parts.len() - 1];
        if matches!(object_entry_kind(parent), EntryKind::Directory)
            || matches!(range_entry_kind(parent), EntryKind::Directory)
        {
            return EntryKind::Temporary;
        }
    }
    kind
}

pub(crate) fn payload_path(relative: &Path) -> Option<&Path> {
    let mut components = relative.components();
    if components.next() != Some(Component::Normal(std::ffi::OsStr::new("namespaces"))) {
        return Some(relative);
    }
    let Component::Normal(namespace) = components.next()? else {
        return None;
    };
    hex(namespace.to_str()?, 64).then_some(components.as_path())
}

pub(crate) fn object_entry_kind(parts: &[&str]) -> EntryKind {
    match parts {
        [
            "chunks" | "xorbs" | "shards" | "ref-transactions" | "git-packs" | "stages"
            | "manifests" | "hints" | "responses",
        ] => EntryKind::Directory,
        ["hints", "clean-bloom.bin"] => EntryKind::Payload,
        ["manifests", name]
            if [".json", ".etag"].iter().any(|suffix| {
                name.strip_suffix(suffix)
                    .is_some_and(|stem| !stem.is_empty())
            }) =>
        {
            EntryKind::Payload
        }
        [
            "chunks" | "xorbs" | "shards" | "ref-transactions" | "git-packs" | "stages",
            prefix,
        ] if hex(prefix, 2) => EntryKind::Directory,
        [
            "chunks" | "xorbs" | "shards" | "ref-transactions" | "git-packs" | "stages",
            prefix,
            hash,
        ] if hex(prefix, 2) && hex(hash, 64) && hash.starts_with(prefix) => EntryKind::Payload,
        _ => EntryKind::Retain,
    }
}

pub(crate) fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(feature = "xet-chunk-cache")]
pub(crate) fn range_entry_kind(parts: &[&str]) -> EntryKind {
    let valid_key = |prefix: &str, key: &str| {
        prefix.strip_prefix("r-").is_some_and(|bucket| {
            hex(bucket, 2)
                && key.starts_with(bucket)
                && crate::xet_chunk_cache::decode_range_name(key)
                    .is_some_and(|bytes| bytes.len() >= 32)
        })
    };
    match parts {
        ["chunks", prefix]
            if prefix
                .strip_prefix("r-")
                .is_some_and(|bucket| hex(bucket, 2)) =>
        {
            EntryKind::Directory
        }
        ["chunks", prefix, key] if valid_key(prefix, key) => EntryKind::Directory,
        ["chunks", prefix, key, item]
            if valid_key(prefix, key)
                && crate::xet_chunk_cache::decode_range_item_name(std::ffi::OsStr::new(item))
                    .is_some() =>
        {
            EntryKind::Payload
        }
        _ => EntryKind::Retain,
    }
}

#[cfg(not(feature = "xet-chunk-cache"))]
fn range_entry_kind(_parts: &[&str]) -> EntryKind {
    EntryKind::Retain
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::private_fs;

    #[tokio::test]
    async fn cleanup_preserves_unknown_live_and_database_owners() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let hash = "ab".repeat(32);
        let payloads = [
            format!("chunks/ab/{hash}"),
            format!("shards/ab/{hash}"),
            format!("xorbs/ab/{hash}"),
            format!("ref-transactions/ab/{hash}"),
            format!("stages/ab/{hash}"),
            "manifests/repo.json".into(),
            "manifests/repo.etag".into(),
        ];
        let retained = [
            ".catalog.sqlite",
            ".catalog.sqlite-wal",
            ".catalog.sqlite-shm",
            ".catalog.sqlite-owner",
            "xorb-index/index.sqlite",
            "repos/repo/chunk-index.sqlite",
            "buckets/bucket/index.sqlite",
            "mirrors/repo.git/HEAD",
            "repack/workspace/output",
            "profiles/session.json",
            "bloom.bin",
            "unknown/user-file",
            "chunks/ab/notes.txt",
            "manifests/.tmp-123-1",
        ];
        for path in payloads.iter().map(String::as_str).chain(retained) {
            private_fs::atomic_write(&root, &root.join(path), b"fixture")
                .await
                .unwrap();
        }
        let preview = clean_cache(&root, true, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!((preview.files_removed, preview.bytes_reclaimed), (7, 49));
        for path in &payloads {
            assert_eq!(std::fs::read(root.join(path)).unwrap(), b"fixture");
        }
        let report = clean_cache(&root, false, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!((report.files_removed, report.bytes_reclaimed), (7, 49));
        assert_eq!(report.retained_entries, retained.len() as u64);
        for path in retained {
            assert_eq!(
                std::fs::read(root.join(path)).unwrap(),
                b"fixture",
                "{path}"
            );
        }
        assert!(payloads.iter().all(|path| !root.join(path).exists()));
    }

    #[tokio::test]
    async fn cleanup_skips_readers_and_preserves_an_inflight_fill() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let path = root.join(format!("xorbs/ab/{}", "ab".repeat(32)));
        private_fs::atomic_write(&root, &path, b"old")
            .await
            .unwrap();
        let reader = private_fs::open_read(&root, &path).await.unwrap();
        let pending = private_fs::PendingFile::new(&root, &path).await.unwrap();
        let report = clean_cache(&root, false, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            (
                report.files_removed,
                report.busy_entries,
                report.retained_entries
            ),
            (0, 1, 1)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        drop(reader);
        pending.commit().await.unwrap();
        let report = clean_cache(&root, false, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(report.files_removed, 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn cleanup_rejects_root_symlinks_and_skips_unsafe_payloads() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let outside = temp.path().join("outside");
        let path = root.join(format!("chunks/ab/{}", "ab".repeat(32)));
        private_fs::atomic_write(&root, &path, b"private")
            .await
            .unwrap();
        private_fs::atomic_write(&outside, &outside.join("sentinel"), b"outside")
            .await
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&outside, root.join("xorbs")).unwrap();
        let report = clean_cache(&root, false, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!((report.files_removed, report.unsafe_entries), (0, 2));
        assert_eq!(std::fs::read(&path).unwrap(), b"private");
        assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"outside");
        let alias = temp.path().join("alias");
        symlink(&root, &alias).unwrap();
        assert!(matches!(
            clean_cache(&alias, false, &CancellationToken::new()).await,
            Err(CacheError::UnsafeRoot { .. })
        ));
    }

    #[tokio::test]
    async fn missing_and_cancelled_cleanup_do_not_create_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("missing");
        let cancel = CancellationToken::new();
        for dry_run in [true, false] {
            assert_eq!(
                clean_cache(&root, dry_run, &cancel)
                    .await
                    .unwrap()
                    .files_removed,
                0
            );
        }
        cancel.cancel();
        assert!(matches!(
            clean_cache(&root, false, &cancel).await,
            Err(CacheError::Cancelled)
        ));
        assert!(!root.exists());
    }

    #[cfg(feature = "xet-chunk-cache")]
    #[tokio::test]
    async fn cleanup_removes_ranges_published_by_the_real_range_cache() {
        use xet_client::cas_types::{ChunkRange, Key};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let range_catalog = crate::CacheCatalog::new(root.clone(), 1024 * 1024);
        let handle = crate::XetChunkCacheHandle::open(root.join("chunks"), range_catalog).unwrap();
        let key = Key {
            prefix: "repo".into(),
            hash: crab_xet::hash::compute_data_hash(b"fixture"),
        };
        let range = ChunkRange::new(0, 1);
        handle
            .cache
            .put(&key, &range, &[0, 7], b"fixture")
            .await
            .unwrap();
        let report = clean_cache(&root, false, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(report.files_removed, 1);
        assert!(handle.cache.get(&key, &range).await.unwrap().is_none());
    }
}

#[cfg(all(test, feature = "local-cache"))]
mod namespace_tests {
    use super::*;
    use crate::{CacheCatalog, CacheKey, LocalCache};

    #[tokio::test]
    async fn namespace_cleanup_retires_shared_accounting_and_keeps_unknown_state() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("content");
        crate::ensure_private_cache_directory(&root).unwrap();
        let key = CacheKey::RefTransaction(blake3::hash(b"data"));
        let catalog = CacheCatalog::new(root.clone(), None);
        let mut caches = Vec::new();
        for namespace in ["ab", "cd"] {
            let cache = LocalCache::namespaced(catalog.clone(), &namespace.repeat(32)).unwrap();
            cache.put(&key, b"data").await.unwrap();
            caches.push(cache);
        }
        let retained = root.join("database-query/chunks/user-file");
        crate::private_fs::atomic_write(&root, &retained, b"query state")
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let before = CacheCatalog::read_only_stats(&root).unwrap();
        let preview = clean_cache(&root, true, &cancel).await.unwrap();
        assert_eq!((preview.files_removed, preview.bytes_reclaimed), (2, 8));
        assert_eq!(CacheCatalog::read_only_stats(&root).unwrap(), before);
        for cache in &caches {
            assert!(cache.contains(&key).await);
            assert!(!cache.root().join(".catalog.sqlite").exists());
        }
        let report = clean_cache(&root, false, &cancel).await.unwrap();
        assert_eq!((report.files_removed, report.bytes_reclaimed), (2, 8));
        assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().entries, 0);
        for cache in &caches {
            assert!(!cache.contains(&key).await);
        }
        assert_eq!(std::fs::read(retained).unwrap(), b"query state");
    }

    #[tokio::test]
    async fn cleanup_preserves_live_catalog_leases() {
        for namespaced in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("content");
            crate::ensure_private_cache_directory(&root).unwrap();
            let catalog = CacheCatalog::new(root.clone(), None);
            let cache = if namespaced {
                LocalCache::namespaced(catalog.clone(), &"ab".repeat(32)).unwrap()
            } else {
                LocalCache::new(root.clone())
            };
            let namespace = cache.root().to_owned();
            let hash = blake3::hash(b"data");
            let key = CacheKey::RefTransaction(hash);
            cache.put(&key, b"data").await.unwrap();
            let hex = hash.to_hex();
            let file = namespace
                .join("ref-transactions")
                .join(&hex[..2])
                .join(hex.as_str());
            let lease = catalog.lease(&file).await.unwrap();
            let cancel = CancellationToken::new();
            for dry_run in [true, false] {
                let report = clean_cache(&root, dry_run, &cancel).await.unwrap();
                assert_eq!((report.files_removed, report.busy_entries), (0, 1));
                assert!(cache.contains(&key).await);
            }
            drop(lease);
            let report = clean_cache(&root, false, &cancel).await.unwrap();
            assert_eq!(report.files_removed, 1);
        }
    }
}
