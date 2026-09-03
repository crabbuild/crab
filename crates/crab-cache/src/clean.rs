//! Ownership policy for explicit local payload cleanup.

use std::path::{Component, Path};

use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::CacheError;
use crate::Result;

/// Cleanup totals; retained subtrees are counted once, without inspecting their contents.
#[derive(Debug, Default, serde::Serialize)]
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
    let root = root.to_owned();
    crate::private_fs::run_blocking(cancel, move |cancel| {
        crate::private_fs::clean(&root, dry_run, cancel)
    })
    .await
}

pub(crate) enum EntryKind {
    Directory,
    Payload,
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
    match object_entry_kind(&parts) {
        EntryKind::Retain => range_entry_kind(&parts),
        kind => kind,
    }
}

pub(crate) fn object_entry_kind(parts: &[&str]) -> EntryKind {
    match parts {
        ["chunks" | "xorbs" | "shards" | "stages" | "manifests" | "hints"] => EntryKind::Directory,
        ["hints", "clean-bloom.bin"] => EntryKind::Payload,
        ["manifests", name]
            if [".json", ".etag"].iter().any(|suffix| {
                name.strip_suffix(suffix)
                    .is_some_and(|stem| !stem.is_empty())
            }) =>
        {
            EntryKind::Payload
        }
        ["chunks" | "xorbs" | "shards" | "stages", prefix] if hex(prefix, 2) => {
            EntryKind::Directory
        }
        ["chunks" | "xorbs" | "shards" | "stages", prefix, hash]
            if hex(prefix, 2) && hex(hash, 64) && hash.starts_with(prefix) =>
        {
            EntryKind::Payload
        }
        _ => EntryKind::Retain,
    }
}

fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(feature = "xet-chunk-cache")]
pub(crate) fn range_entry_kind(parts: &[&str]) -> EntryKind {
    use base64::Engine as _;
    let base64 = base64::engine::general_purpose::URL_SAFE;
    let valid_key = |prefix: &str, key: &str| {
        prefix.len() == 2
            && key.starts_with(prefix)
            && base64.decode(key).is_ok_and(|bytes| bytes.len() >= 32)
    };
    match parts {
        ["chunks", prefix]
            if prefix.len() == 2
                && prefix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
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
        assert_eq!((preview.files_removed, preview.bytes_reclaimed), (6, 42));
        for path in &payloads {
            assert_eq!(std::fs::read(root.join(path)).unwrap(), b"fixture");
        }
        let report = clean_cache(&root, false, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!((report.files_removed, report.bytes_reclaimed), (6, 42));
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
        let handle = crate::XetChunkCacheHandle::open(root.join("chunks"), 1024 * 1024).unwrap();
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
