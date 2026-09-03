//! Actual command cleanup must preserve non-payload owners under the cache root.

#![cfg(unix)]

use std::process::Command;

use crab::cache::{CacheKey, LocalCache};
use crab_xet::hash::compute_data_hash;

#[tokio::test]
async fn cache_clean_commands_remove_payloads_without_erasing_retained_state() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let cache = LocalCache::new(root.clone());
    let key = CacheKey::Chunk(compute_data_hash(b"data"));
    for args in [vec!["cache", "clean"], vec!["optimize", "cache", "clean"]] {
        cache.put(&key, b"data").await.unwrap();
        let sentinel = root.join("retained-workspace/user-file");
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"keep").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_crab"))
            .args(args)
            .current_dir(temp.path())
            .env("CRAB_CACHE_DIR", &root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Removed 1 cache payload(s), 4 B;")
        );
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
        assert!(!cache.contains(&key).await);
    }
}
