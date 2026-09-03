use super::*;
use crate::catalog::{CacheCatalog, PayloadRead};

async fn failed_read(root: &Path) -> (CrabRangeCache, PathBuf, PayloadRead) {
    let cache = CrabRangeCache {
        root: root.join("chunks"),
        capacity: 1024 * 1024,
        catalog: CacheCatalog::new(root.to_owned(), 1024 * 1024),
    };
    cache
        .put(&test_key(), &ChunkRange::new(0, 1), &[0, 4], b"data")
        .await
        .unwrap();
    let path = fs::read_dir(cache.key_directory(&test_key()))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let item = decode_range_item_name(path.file_name().unwrap()).unwrap();
    fs::write(&path, b"bad").unwrap();
    let relative = path.strip_prefix(root).unwrap().to_owned();
    let (entry, reader) = PayloadRead::open(root, &path).await.unwrap();
    assert!(
        CrabRangeCache::read_open_entry(reader, item, &ChunkRange::new(0, 1))
            .await
            .is_err()
    );
    (cache, relative, entry)
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_read_cannot_discard_a_later_publication() {
    for replace_root in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let (cache, relative, entry) = failed_read(&root).await;
        let moved = temp.path().join("moved");
        if replace_root {
            fs::rename(&root, &moved).unwrap();
        }
        // A real publisher repairs the same key while the old failed reader
        // retains its descriptor. Publication is a new inode, not an edit.
        cache
            .put(&test_key(), &ChunkRange::new(0, 1), &[0, 4], b"data")
            .await
            .unwrap();
        let before = CacheCatalog::read_only_stats(&root).unwrap();
        let bytes = fs::read(root.join(&relative)).unwrap();
        entry.discard().await.unwrap();
        assert_eq!(fs::read(root.join(&relative)).unwrap(), bytes);
        assert_eq!(CacheCatalog::read_only_stats(&root).unwrap(), before);
        assert_eq!(
            cache
                .get(&test_key(), &ChunkRange::new(0, 1))
                .await
                .unwrap()
                .unwrap()
                .data,
            b"data"
        );
        if replace_root {
            assert!(!moved.join(relative).exists());
            assert_eq!(CacheCatalog::read_only_stats(&moved).unwrap().entries, 0);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_read_repair_respects_other_live_readers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let (_, relative, entry) = failed_read(&root).await;
    let other_reader = PinnedRoot::open(&root)
        .unwrap()
        .open_read(&relative)
        .unwrap();
    let before = CacheCatalog::read_only_stats(&root).unwrap();
    let result = entry.discard().await;
    assert!(
        matches!(result, Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert_eq!(fs::read(root.join(relative)).unwrap(), b"bad");
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap(), before);
    drop(other_reader);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_read_repair_retires_only_its_own_file() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let (_, relative, entry) = failed_read(&root).await;
    entry.discard().await.unwrap();
    assert!(!root.join(relative).exists());
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap().entries, 0);
}
