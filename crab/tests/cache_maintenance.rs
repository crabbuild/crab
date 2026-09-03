//! Actual maintenance commands must use private payload ownership rules.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

use base64::Engine as _;
use crab::cache::{CacheKey, LocalCache, XetChunkCacheHandle};
use crab_xet::hash::compute_data_hash;
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use crab_xet::xorb::format::Chunk;
use crab_xet::xorb::parser::XorbParser;
use xet_client::cas_types::{ChunkRange, Key};

const MAINTENANCE_COMMANDS: &[&[&str]] = &[
    &["cache", "clean"],
    &["optimize", "cache", "clean"],
    &["cache", "verify"],
    &["optimize", "cache", "verify"],
    &["prune"],
    &["prune", "--dry-run"],
    &["optimize", "cache", "prune"],
    &["optimize", "cache", "prune", "--dry-run"],
];

fn command_output(directory: &Path, root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crab"))
        .args(args)
        .current_dir(directory)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("CRAB_CACHE_DIR", root)
        .output()
        .unwrap()
}

fn command(directory: &Path, root: &Path, args: &[&str]) -> Vec<u8> {
    let output = command_output(directory, root, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[tokio::test]
async fn maintenance_commands_reject_checkout_roots_without_touching_payloads() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    let cache = LocalCache::new(root.clone());
    let hash = compute_data_hash(b"data");
    let key = CacheKey::Chunk(hash);
    cache.put(&key, b"data").await.unwrap();
    let hex = hash.hex();
    let payload = root.join("chunks").join(&hex[..2]).join(&hex);
    // A canonical name is not proof that a broad user directory is a cache.
    // Corrupt content also makes the verify command's destructive path eligible.
    std::fs::write(&payload, b"bad!").unwrap();
    let sentinel = root.join("user-notes");
    std::fs::write(&sentinel, b"keep").unwrap();
    for (directory, relative_root) in [
        (root.clone(), Path::new(".")),
        (root.join("nested"), Path::new("..")),
    ] {
        let config = directory.join(crab::core::config::REPO_CONFIG_REL);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(config, "[cache]\nmax_bytes = 1\n").unwrap();
        for selected_root in [root.as_path(), relative_root] {
            for args in MAINTENANCE_COMMANDS {
                let output = command_output(&directory, selected_root, args);
                assert!(
                    !output.status.success(),
                    "{args:?} accepted checkout root {selected_root:?}"
                );
                assert!(
                    String::from_utf8_lossy(&output.stderr)
                        .contains("cache directory is unsafe for cleanup"),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(std::fs::read(&payload).unwrap(), b"bad!");
                assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
            }
        }
    }
}

#[test]
fn maintenance_commands_leave_missing_roots_missing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing");
    for args in MAINTENANCE_COMMANDS {
        command(temp.path(), &root, args);
        assert!(!root.exists(), "{args:?} created a cache root");
    }
}

#[tokio::test]
async fn object_maintenance_commands_preserve_unknown_state_and_remote_proofs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let config = temp.path().join(crab::core::config::REPO_CONFIG_REL);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "[cache]\nmax_bytes = 1\n").unwrap();
    let cache = LocalCache::new(root.clone());
    let hash = compute_data_hash(b"data");
    let mut builder = XorbBuilder::new();
    builder
        .push(&Chunk::new(bytes::Bytes::from_static(b"data")), RunId(0))
        .unwrap();
    let xorb = builder.finalize().unwrap().pop().unwrap();
    let digest = XorbParser::parse(xorb.bytes.clone())
        .unwrap()
        .payload_digest();
    let objects = [
        ("chunks", hash, CacheKey::Chunk(hash), b"data".as_slice()),
        ("shards", hash, CacheKey::Shard(hash), b"data".as_slice()),
        (
            "xorbs",
            xorb.hash,
            CacheKey::Xorb(xorb.hash),
            xorb.bytes.as_ref(),
        ),
    ];
    for (_, _, key, data) in &objects {
        cache.put(key, data).await.unwrap();
    }
    assert!(
        cache
            .record_remote_xorb_proof(
                &xorb.hash,
                &digest,
                xorb.bytes.len() as u64,
                Some("origin-etag"),
                None
            )
            .unwrap()
    );
    let mut retained = Vec::new();
    for (family, hash, _, _) in &objects {
        let hex = hash.hex();
        let parent = root.join(family).join(&hex[..2]);
        std::fs::write(parent.join(hex), b"corrupt").unwrap();
        let sentinel = parent.join("notes.txt");
        std::fs::write(&sentinel, b"sentinel").unwrap();
        retained.push(sentinel);
    }
    let output = command(temp.path(), &root, &["cache", "verify"]);
    assert_eq!(
        String::from_utf8(output).unwrap().trim(),
        "Checked 3 cache object(s): 0 valid, 3 corrupt evicted"
    );
    for (_, _, key, data) in &objects {
        assert!(!cache.contains(key).await);
        cache.put(key, data).await.unwrap();
    }
    let expected_bytes = 8 + xorb.bytes.len() as u64;
    let preview: serde_json::Value = serde_json::from_slice(&command(
        temp.path(),
        &root,
        &["prune", "--dry-run", "--json"],
    ))
    .unwrap();
    assert_eq!(preview["data"]["objects_pruned"], 3);
    assert_eq!(preview["data"]["bytes_freed"], expected_bytes);
    for (_, _, key, _) in &objects {
        assert!(cache.contains(key).await);
    }
    let applied: serde_json::Value =
        serde_json::from_slice(&command(temp.path(), &root, &["prune", "--json"])).unwrap();
    assert_eq!(applied["data"]["objects_pruned"], 3);
    assert_eq!(applied["data"]["bytes_freed"], expected_bytes);
    for (_, _, key, _) in &objects {
        assert!(!cache.contains(key).await);
    }
    assert!(
        cache
            .remote_xorb_proof_matches(
                &xorb.hash,
                &digest,
                xorb.bytes.len() as u64,
                Some("origin-etag"),
                None
            )
            .unwrap()
    );
    for path in retained {
        assert_eq!(std::fs::read(path).unwrap(), b"sentinel");
    }
}

#[tokio::test]
async fn verify_and_prune_commands_preserve_non_range_owners() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let config = temp.path().join(crab::core::config::REPO_CONFIG_REL);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "[cache]\nmax_bytes = 1\n").unwrap();
    let range_root = root.join("chunks");
    let cache = XetChunkCacheHandle::open(&range_root, 1024 * 1024).unwrap();
    let key = Key {
        prefix: crab_cache::xet_chunk_cache::CHUNK_HASH_PREFIX.into(),
        hash: (*blake3::hash(b"good").as_bytes()).into(),
    };
    let range = ChunkRange::new(0, 1);
    cache
        .cache
        .put(&key, &range, &[0, 4], b"bad!")
        .await
        .unwrap();
    let mut encoded = key.hash.as_bytes().to_vec();
    encoded.extend_from_slice(key.prefix.as_bytes());
    let encoded = base64::engine::general_purpose::URL_SAFE.encode(encoded);
    let entry_dir = range_root.join(&encoded[..2]).join(encoded);
    let payload = std::fs::read_dir(&entry_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let retained = [entry_dir.join("notes.txt"), entry_dir.join(".tmp-sentinel")];
    for path in &retained {
        std::fs::write(path, b"sentinel").unwrap();
    }
    let report = command(temp.path(), &root, &["cache", "verify"]);
    assert_eq!(
        String::from_utf8(report).unwrap().trim(),
        "Checked 1 cache object(s): 0 valid, 1 corrupt evicted"
    );
    assert!(!payload.exists());

    cache
        .cache
        .put(&key, &range, &[0, 4], b"good")
        .await
        .unwrap();
    let preview: serde_json::Value = serde_json::from_slice(&command(
        temp.path(),
        &root,
        &["prune", "--dry-run", "--json"],
    ))
    .unwrap();
    assert_eq!(preview["data"]["objects_pruned"], 1);
    assert_eq!(preview["data"]["bytes_freed"], 16);
    assert!(cache.cache.get(&key, &range).await.unwrap().is_some());
    let applied: serde_json::Value =
        serde_json::from_slice(&command(temp.path(), &root, &["prune", "--json"])).unwrap();
    assert_eq!(applied["data"]["objects_pruned"], 1);
    assert_eq!(applied["data"]["bytes_freed"], 16);
    assert!(cache.cache.get(&key, &range).await.unwrap().is_none());
    for path in retained {
        assert_eq!(std::fs::read(path).unwrap(), b"sentinel");
    }
}
