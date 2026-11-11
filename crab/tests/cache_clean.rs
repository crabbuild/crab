//! Actual command cleanup must preserve non-payload owners under the cache root.

#![cfg(unix)]

use std::process::Command;

use crab::cache::{CacheKey, LocalCache};
use crab_xet::hash::compute_data_hash;

fn isolate_range_cache(directory: &std::path::Path, cache: &std::path::Path) {
    let config = directory.join(".crab");
    std::fs::create_dir_all(&config).unwrap();
    let text = toml::to_string(
        &serde_json::json!({ "cache": { "chunk_cache_dir": cache.join("chunks") } }),
    )
    .unwrap();
    std::fs::write(config.join("config.toml"), text).unwrap();
}

#[tokio::test]
async fn structured_cleanup_previews_and_removes_only_eligible_payloads() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    isolate_range_cache(temp.path(), &root);
    let cache = LocalCache::new(root.clone());
    let key = CacheKey::Chunk(compute_data_hash(b"data"));
    let sentinel = root.join("retained-workspace/user-file");
    for prefix in [vec!["cache", "clean"], vec!["optimize", "cache", "clean"]] {
        cache.put(&key, b"data").await.unwrap();
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"keep").unwrap();
        for dry_run in [true, false] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_crab"));
            command.args(&prefix).arg("--json");
            if dry_run {
                command.arg("--dry-run");
            }
            let output = command
                .current_dir(temp.path())
                .env("CRAB_CACHE_DIR", &root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(envelope["schema"], "cache.clean");
            assert_eq!(envelope["version"], "1.0");
            assert_eq!(envelope["data"]["dry_run"], dry_run);
            assert_eq!(envelope["data"]["bytes_reclaimed"], 4);
            assert_eq!(cache.contains(&key).await, dry_run);
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
        }
    }
}

#[tokio::test]
async fn cache_clean_commands_remove_payloads_without_erasing_retained_state() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    isolate_range_cache(temp.path(), &root);
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

#[tokio::test]
async fn cache_clean_commands_refuse_active_mirror_before_removing_payloads() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    isolate_range_cache(temp.path(), &root);
    let cache = LocalCache::new(root.clone());
    let key = CacheKey::Chunk(compute_data_hash(b"data"));
    cache.put(&key, b"data").await.unwrap();
    let owner = crab_cache::lifecycle::CacheUseGuard::acquire(
        &root.join("mirrors/repo.git"),
        &tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    for args in [
        ["cache", "clean"].as_slice(),
        &["optimize", "cache", "clean"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_crab"))
            .args(args)
            .current_dir(temp.path())
            .env("CRAB_CACHE_DIR", &root)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(cache.contains(&key).await, "{args:?}");
    }
    drop(owner);
    crab_cache::clean_cache(&root, false, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    assert!(!cache.contains(&key).await);
}
