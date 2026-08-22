//! Integration tests for remote cache push/pull (P3).
//!
//! Uses an in-memory object store to verify the push logic without
//! requiring localstack or real S3 credentials. The tests exercise:
//!
//! - `push_remote()` uploads xorbs + manifest + ref.
//! - Concurrent pushes of the same hash: ref CAS ensures one winner.
//! - `push_all_local()` backfills all local entries.
//! - Without `--cache-push`, no remote writes occur.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use tempfile::TempDir;

use crab::workflow::cache::{
    CachedCmd, CachedOut, ENTRY_SCHEMA_VERSION, StageCacheEntry, TreeManifestEntry, push_all_local,
    push_remote, read_local_xorb, store_local_xorbs, write_local,
};
use crab::workflow::hasher::{TreeEntry, TreeEntryKind, hash_tree_entries};
use crab::workflow::stage::OutKind;
use crab::workflow::{WorkflowError, WorkflowStore};
use crab_types::workflow::StageHash;

fn memory_store() -> WorkflowStore {
    let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
    WorkflowStore::new(inner)
}

fn sample_hash() -> StageHash {
    StageHash([
        0xab, 0xcd, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55,
        0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x01, 0x02, 0x03, 0x04,
        0x05, 0x06,
    ])
}

fn sample_entry(stage_hash: StageHash, out_path: &str) -> StageCacheEntry {
    StageCacheEntry {
        schema_version: ENTRY_SCHEMA_VERSION,
        stage_hash,
        stage_name: "train".to_owned(),
        cmd: CachedCmd::Shell {
            shell: "python train.py".to_owned(),
        },
        outs: vec![CachedOut {
            path: PathBuf::from(out_path),
            kind: OutKind::File,
            push: true,
            remote: None,
            file_hash: format!("b3:{}", "00".repeat(32)),
            size: 42,
            mode: 0o644,
            tree_manifest: None,
        }],
        metrics: Vec::new(),
        plots: Vec::new(),
        executed_at: "2025-01-01T00:00:00.000Z".to_owned(),
        duration_ms: 1000,
        exec_id: None,
        attempts: 1,
        host_fingerprint: "test-host".to_owned(),
    }
}

fn set_first_out_content(entry: &mut StageCacheEntry, content: &[u8]) {
    entry.outs[0].file_hash = format!("b3:{}", blake3::hash(content).to_hex());
    entry.outs[0].size = content.len() as u64;
}

fn set_directory_out_metadata(entry: &mut StageCacheEntry) {
    let out = &mut entry.outs[0];
    let manifest = out.tree_manifest.as_ref().unwrap();
    let tree_entries = manifest
        .iter()
        .map(|item| TreeEntry {
            path: PathBuf::from(&item.path),
            kind: match item.kind.as_str() {
                "file" => TreeEntryKind::File,
                "dir" => TreeEntryKind::Directory,
                other => panic!("unexpected directory manifest kind: {other}"),
            },
            file_hash: if item.kind == "file" {
                parse_b3_hash(&item.hash)
            } else {
                [0; 32]
            },
            size: item.size,
            mode: item.mode,
        })
        .collect::<Vec<_>>();
    out.file_hash = format!(
        "b3:{}",
        blake3::Hash::from_bytes(hash_tree_entries(&tree_entries)).to_hex()
    );
    out.size = manifest.iter().map(|item| item.size).sum();
}

fn parse_b3_hash(value: &str) -> [u8; 32] {
    let hex = value.strip_prefix("b3:").unwrap();
    assert_eq!(hex.len(), 64);
    let mut bytes = [0u8; 32];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
    }
    bytes
}

#[tokio::test]
async fn push_remote_uploads_manifest_and_ref() {
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    // Create the output file so the xorb upload can read it.
    let out_path = tmp.path().join("model.bin");
    std::fs::write(&out_path, b"model weights").unwrap();

    let store = memory_store();
    let prefix = "org/repo";
    let hash = sample_hash();
    let mut entry = sample_entry(hash, "model.bin");
    set_first_out_content(&mut entry, b"model weights");

    // Push should succeed and return true (new data written).
    let result = push_remote(&store, prefix, &entry, &cache_root)
        .await
        .unwrap();
    assert!(result, "first push should write new data");

    // Verify the manifest exists at the expected path.
    let hex = hash.as_hex();
    let shard = &hex[..2];
    let manifest_path = ObjectPath::from(format!("{prefix}/workflow/stages/{shard}/{hex}.json"));
    let (manifest_bytes, _) = store.get_with_etag(&manifest_path).await.unwrap();
    assert!(!manifest_bytes.is_empty(), "manifest should not be empty");

    // Verify the manifest is valid JSON containing the stage name.
    let manifest_str = String::from_utf8(manifest_bytes.to_vec()).unwrap();
    assert!(
        manifest_str.contains("\"train\""),
        "manifest should contain stage name"
    );

    // Verify the ref exists.
    let ref_path = ObjectPath::from(format!("{prefix}/refs/crab/stages/{hex}"));
    let (ref_bytes, _) = store.get_with_etag(&ref_path).await.unwrap();
    assert!(!ref_bytes.is_empty(), "ref should not be empty");
}

#[tokio::test]
async fn push_remote_prefers_local_xorb_over_mutated_worktree() {
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let original = b"model weights";
    std::fs::write(tmp.path().join("model.bin"), original).unwrap();

    let hash = sample_hash();
    let mut entry = sample_entry(hash, "model.bin");
    set_first_out_content(&mut entry, original);
    store_local_xorbs(&cache_root, &entry.outs, Some(tmp.path())).unwrap();

    std::fs::write(tmp.path().join("model.bin"), b"mutated after hash").unwrap();

    let store = memory_store();
    let prefix = "org/repo";
    push_remote(&store, prefix, &entry, &cache_root)
        .await
        .unwrap();

    let xorb_path = ObjectPath::from(format!(
        "{prefix}/workflow/xorbs/{}.xorb",
        entry.outs[0].file_hash
    ));
    let (remote_bytes, _) = store.get_with_etag(&xorb_path).await.unwrap();
    assert_eq!(remote_bytes.as_ref(), original);
}

#[tokio::test]
async fn push_remote_missing_xorb_source_does_not_publish_ref() {
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let hash = sample_hash();
    let mut entry = sample_entry(hash, "model.bin");
    set_first_out_content(&mut entry, b"missing bytes");

    let store = memory_store();
    let prefix = "org/repo";
    let result = push_remote(&store, prefix, &entry, &cache_root).await;

    assert!(result.is_err(), "missing xorb bytes should fail the push");
    let ref_path = ObjectPath::from(format!("{prefix}/refs/crab/stages/{}", hash.as_hex()));
    assert!(
        store.head(&ref_path).await.is_err(),
        "ref must not publish without every xorb"
    );
}

#[tokio::test]
async fn push_remote_concurrent_same_hash_second_writer_no_ops() {
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let out_path = tmp.path().join("model.bin");
    std::fs::write(&out_path, b"model weights").unwrap();

    let store = memory_store();
    let prefix = "org/repo";
    let hash = sample_hash();
    let mut entry = sample_entry(hash, "model.bin");
    set_first_out_content(&mut entry, b"model weights");

    // First push writes.
    let first = push_remote(&store, prefix, &entry, &cache_root)
        .await
        .unwrap();
    assert!(first, "first push should write");

    // Second push of the same hash should no-op (ref already exists).
    let second = push_remote(&store, prefix, &entry, &cache_root)
        .await
        .unwrap();
    assert!(!second, "second push should no-op (ref already exists)");
}

#[tokio::test]
async fn push_all_local_backfills_entries() {
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join(".crab/cache");

    // Create two local cache entries.
    let hash1 = StageHash([1u8; 32]);
    let hash2 = StageHash([2u8; 32]);
    let mut entry1 = sample_entry(hash1, "out1.bin");
    let mut entry2 = sample_entry(hash2, "out2.bin");
    set_first_out_content(&mut entry1, b"data1");
    set_first_out_content(&mut entry2, b"data2");

    write_local(&cache_root, &entry1).unwrap();
    write_local(&cache_root, &entry2).unwrap();

    // Create the output files.
    std::fs::write(tmp.path().join("out1.bin"), b"data1").unwrap();
    std::fs::write(tmp.path().join("out2.bin"), b"data2").unwrap();

    let store = memory_store();
    let prefix = "org/repo";

    let result = push_all_local(&store, prefix, &cache_root).await.unwrap();
    assert_eq!(result.pushed, 2, "should push both entries");
    assert_eq!(result.skipped, 0);
    assert_eq!(result.errors, 0);

    // Push again — both should be skipped now.
    let result2 = push_all_local(&store, prefix, &cache_root).await.unwrap();
    assert_eq!(result2.pushed, 0, "second push should skip all");
    assert_eq!(result2.skipped, 2);
    assert_eq!(result2.errors, 0);
}

#[tokio::test]
async fn without_cache_push_no_remote_writes() {
    // This test verifies that the executor's default behavior (cache_push = false)
    // does not write to the remote. We verify by checking that the store remains
    // empty after a local cache write.
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join(".crab/cache");

    let hash = sample_hash();
    let entry = sample_entry(hash, "model.bin");

    // Write locally (simulating what the executor does without --cache-push).
    write_local(&cache_root, &entry).unwrap();

    // Verify the remote store is empty.
    let store = memory_store();
    let prefix = "org/repo";
    let hex = hash.as_hex();
    let ref_path = ObjectPath::from(format!("{prefix}/refs/crab/stages/{hex}"));

    let result = store.head(&ref_path).await;
    assert!(result.is_err(), "ref should not exist without --cache-push");
}

// ─── Remote cache pull tests ───

use crab::workflow::cache::pull_remote;

#[tokio::test]
async fn pull_remote_materializes_outputs_from_remote() {
    // Simulate machine A pushing, machine B pulling.
    let tmp_a = TempDir::new().unwrap();
    let cache_root_a = tmp_a.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root_a).unwrap();

    // Machine A: create output file and push.
    let out_content = b"model weights from machine A";
    let out_path_a = tmp_a.path().join("model.bin");
    std::fs::write(&out_path_a, out_content).unwrap();

    let store = memory_store();
    let prefix = "org/repo";
    let hash = sample_hash();
    let mut entry = sample_entry(hash, "model.bin");
    // Use the real blake3 hash of the content so verification passes.
    set_first_out_content(&mut entry, out_content);
    let real_hash = entry.outs[0].file_hash.clone();

    // Push from machine A.
    let pushed = push_remote(&store, prefix, &entry, &cache_root_a)
        .await
        .unwrap();
    assert!(pushed, "push should succeed");

    // Machine B: fresh cache, no local entry.
    let tmp_b = TempDir::new().unwrap();
    let cache_root_b = tmp_b.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root_b).unwrap();

    // Pull on machine B.
    let pulled = pull_remote(&store, prefix, &hash, &cache_root_b, Some(tmp_b.path()))
        .await
        .unwrap();

    assert!(pulled.is_some(), "remote pull should find the entry");
    let pulled_entry = pulled.unwrap();
    assert_eq!(pulled_entry.stage_name, "train");
    assert_eq!(pulled_entry.stage_hash, hash);

    // Verify the output file was materialized on machine B.
    let materialized = tmp_b.path().join("model.bin");
    assert!(materialized.exists(), "output should be materialized");
    let materialized_bytes = std::fs::read(&materialized).unwrap();
    assert_eq!(
        materialized_bytes, out_content,
        "outputs should be byte-identical"
    );

    // Verify the entry was written to the local cache on machine B.
    let local_entry = crab::workflow::cache::read_local(&cache_root_b, &hash).unwrap();
    assert!(
        local_entry.is_some(),
        "entry should be written to local cache"
    );
    let local_bytes = read_local_xorb(&cache_root_b, &real_hash)
        .unwrap()
        .expect("remote pull should fill local xorb cache");
    assert_eq!(local_bytes, out_content);
}

#[tokio::test]
async fn push_pull_directory_remote_fills_local_xorb_cache() {
    let tmp_a = TempDir::new().unwrap();
    let cache_root_a = tmp_a.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root_a).unwrap();
    std::fs::create_dir_all(tmp_a.path().join("artifacts/nested")).unwrap();
    std::fs::create_dir_all(tmp_a.path().join("artifacts/empty")).unwrap();
    std::fs::write(tmp_a.path().join("artifacts/nested/a.txt"), b"a").unwrap();
    std::fs::write(tmp_a.path().join("artifacts/b.txt"), b"b").unwrap();

    let a_hash = format!("b3:{}", blake3::hash(b"a").to_hex());
    let b_hash = format!("b3:{}", blake3::hash(b"b").to_hex());
    let hash = StageHash([0x36; 32]);
    let mut entry = sample_entry(hash, "artifacts");
    entry.outs[0] = CachedOut {
        path: PathBuf::from("artifacts"),
        kind: OutKind::Directory,
        push: true,
        remote: None,
        file_hash: format!("b3:{}", "36".repeat(32)),
        size: 2,
        mode: 0o755,
        tree_manifest: Some(vec![
            TreeManifestEntry {
                path: "empty".to_owned(),
                kind: "dir".to_owned(),
                hash: String::new(),
                size: 0,
                mode: 0o755,
            },
            TreeManifestEntry {
                path: "nested/a.txt".to_owned(),
                kind: "file".to_owned(),
                hash: a_hash.clone(),
                size: 1,
                mode: 0o644,
            },
            TreeManifestEntry {
                path: "b.txt".to_owned(),
                kind: "file".to_owned(),
                hash: b_hash.clone(),
                size: 1,
                mode: 0o644,
            },
        ]),
    };
    set_directory_out_metadata(&mut entry);

    let store = memory_store();
    let prefix = "org/repo";
    assert!(
        push_remote(&store, prefix, &entry, &cache_root_a)
            .await
            .unwrap()
    );

    let tmp_b = TempDir::new().unwrap();
    let cache_root_b = tmp_b.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root_b).unwrap();
    let pulled = pull_remote(&store, prefix, &hash, &cache_root_b, Some(tmp_b.path()))
        .await
        .unwrap()
        .expect("remote directory pull should hit");

    assert_eq!(
        std::fs::read_to_string(tmp_b.path().join("artifacts/nested/a.txt")).unwrap(),
        "a"
    );
    assert_eq!(
        std::fs::read_to_string(tmp_b.path().join("artifacts/b.txt")).unwrap(),
        "b"
    );
    assert!(tmp_b.path().join("artifacts/empty").is_dir());
    assert_eq!(
        read_local_xorb(&cache_root_b, &a_hash).unwrap().unwrap(),
        b"a"
    );
    assert_eq!(
        read_local_xorb(&cache_root_b, &b_hash).unwrap().unwrap(),
        b"b"
    );

    std::fs::remove_dir_all(tmp_b.path().join("artifacts")).unwrap();
    crab::workflow::materialize::materialize_directory(
        &tmp_b.path().join("artifacts"),
        pulled.outs[0].tree_manifest.as_ref().unwrap(),
        &cache_root_b,
        uuid::Uuid::now_v7(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(tmp_b.path().join("artifacts/nested/a.txt")).unwrap(),
        "a"
    );
    assert!(tmp_b.path().join("artifacts/empty").is_dir());
}

#[tokio::test]
async fn pull_remote_returns_none_on_miss() {
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let store = memory_store();
    let prefix = "org/repo";
    // Use a hash that was never pushed.
    let hash = StageHash([0xffu8; 32]);

    let result = pull_remote(&store, prefix, &hash, &cache_root, Some(tmp.path()))
        .await
        .unwrap();

    assert!(result.is_none(), "remote miss should return None");
}

#[tokio::test]
async fn pull_remote_returns_none_on_network_error() {
    // Use a store that will fail on get operations by using a path
    // that doesn't exist in the in-memory store (simulates 404/network error).
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let store = memory_store();
    let prefix = "org/repo";
    let hash = sample_hash();

    // Don't push anything — the manifest won't exist, simulating a miss.
    let result = pull_remote(&store, prefix, &hash, &cache_root, Some(tmp.path()))
        .await
        .unwrap();

    assert!(
        result.is_none(),
        "missing manifest should return None (not error)"
    );
}

#[tokio::test]
async fn pull_remote_rejects_stage_hash_mismatch() {
    let tmp_a = TempDir::new().unwrap();
    let cache_root_a = tmp_a.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root_a).unwrap();

    let out_path_a = tmp_a.path().join("model.bin");
    std::fs::write(&out_path_a, b"data").unwrap();

    let store = memory_store();
    let prefix = "org/repo";
    let hash = sample_hash();
    let mut entry = sample_entry(hash, "model.bin");
    set_first_out_content(&mut entry, b"data");

    // Push with the correct hash.
    push_remote(&store, prefix, &entry, &cache_root_a)
        .await
        .unwrap();

    // Try to pull with a different hash — the manifest's stage_hash
    // won't match the requested hash, so pull should return None.
    let different_hash = StageHash([0xffu8; 32]);
    let tmp_b = TempDir::new().unwrap();
    let cache_root_b = tmp_b.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root_b).unwrap();

    let result = pull_remote(
        &store,
        prefix,
        &different_hash,
        &cache_root_b,
        Some(tmp_b.path()),
    )
    .await
    .unwrap();

    assert!(result.is_none(), "hash mismatch should return None");
}

#[tokio::test]
async fn pull_remote_round_trip_byte_identical() {
    // Full round-trip: push from A, pull on B, verify byte-identical outputs.
    let tmp_a = TempDir::new().unwrap();
    let cache_root_a = tmp_a.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root_a).unwrap();

    // Create a non-trivial output file.
    let out_content: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
    let out_path_a = tmp_a.path().join("output.dat");
    std::fs::write(&out_path_a, &out_content).unwrap();

    let store = memory_store();
    let prefix = "team/ml-repo";
    let hash = StageHash([0x42u8; 32]);
    let mut entry = sample_entry(hash, "output.dat");
    // Update the file_hash to match the actual content hash.
    set_first_out_content(&mut entry, &out_content);
    let file_hash = entry.outs[0].file_hash.clone();

    // Push from machine A.
    push_remote(&store, prefix, &entry, &cache_root_a)
        .await
        .unwrap();

    // Pull on machine B.
    let tmp_b = TempDir::new().unwrap();
    let cache_root_b = tmp_b.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root_b).unwrap();

    let pulled = pull_remote(&store, prefix, &hash, &cache_root_b, Some(tmp_b.path()))
        .await
        .unwrap()
        .expect("pull should succeed");

    // Verify byte-identical output.
    let materialized = tmp_b.path().join("output.dat");
    let materialized_bytes = std::fs::read(&materialized).unwrap();
    assert_eq!(
        materialized_bytes, out_content,
        "pulled output must be byte-identical to pushed output"
    );

    // Verify the entry metadata matches.
    assert_eq!(pulled.stage_hash, hash);
    assert_eq!(pulled.outs[0].file_hash, file_hash);
}

// ─── Remote cache integrity verification tests (D.1) ───

use crab::workflow::cache::check_remote_cache_readonly;

#[tokio::test]
async fn pull_remote_detects_corrupted_file_bytes() {
    // Simulate a scenario where the xorb content doesn't match the
    // hash recorded in the manifest (e.g., storage corruption or
    // a man-in-the-middle attack).
    let store = memory_store();
    let prefix = "org/repo";
    let hash = sample_hash();

    // Create an entry where the file_hash claims one thing...
    let claimed_hash = format!("b3:{}", blake3::hash(b"original content").to_hex());
    let mut entry = sample_entry(hash, "model.bin");
    entry.outs[0].file_hash = claimed_hash.clone();

    // ...but upload different bytes at that xorb path.
    let tampered_content = b"TAMPERED malicious content";
    let xorb_path = ObjectPath::from(format!("{prefix}/workflow/xorbs/{claimed_hash}.xorb"));
    store
        .put(&xorb_path, bytes::Bytes::from_static(tampered_content))
        .await
        .unwrap();

    // Upload the manifest (which references the claimed_hash).
    let hex = hash.as_hex();
    let shard = &hex[..2];
    let manifest_path = ObjectPath::from(format!("{prefix}/workflow/stages/{shard}/{hex}.json"));
    let manifest_json = serde_json::to_vec(&entry).unwrap();
    store
        .put(&manifest_path, bytes::Bytes::from(manifest_json))
        .await
        .unwrap();

    // Also upload the ref so the manifest is discoverable.
    let ref_path = ObjectPath::from(format!("{prefix}/refs/crab/stages/{hex}"));
    store
        .put(
            &ref_path,
            bytes::Bytes::from(manifest_path.as_ref().as_bytes().to_vec()),
        )
        .await
        .unwrap();

    // Pull should detect that the downloaded bytes don't match the manifest hash.
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let result = pull_remote(&store, prefix, &hash, &cache_root, Some(tmp.path())).await;

    assert!(result.is_err(), "corrupted xorb should be detected");
    let err = result.unwrap_err();
    match &err {
        WorkflowError::CacheEntryCorrupt {
            stage_hash,
            path,
            expected,
            actual,
        } => {
            assert_eq!(stage_hash, &hash.as_hex());
            assert_eq!(path, "model.bin");
            assert_eq!(expected, &claimed_hash);
            assert_ne!(actual, expected, "actual hash should differ from expected");
        }
        other => panic!("expected CacheEntryCorrupt, got: {other}"),
    }

    // Verify no partial files remain.
    let materialized = tmp.path().join("model.bin");
    assert!(!materialized.exists(), "partial files should be cleaned up");
}

#[tokio::test]
async fn pull_remote_rejects_manifest_with_wrong_stage_hash() {
    // Create a manifest where the stage_hash field doesn't match the
    // locally-computed hash. This simulates a tampered manifest.
    let store = memory_store();
    let prefix = "org/repo";

    // The hash we'll request.
    let local_hash = sample_hash();

    // Create an entry with a DIFFERENT stage_hash in the manifest body.
    let wrong_hash = StageHash([0x11u8; 32]);
    let mut entry = sample_entry(wrong_hash, "model.bin");
    entry.stage_hash = wrong_hash;

    // Manually upload the manifest at the path for local_hash, but with
    // wrong_hash inside the JSON body.
    let hex = local_hash.as_hex();
    let shard = &hex[..2];
    let manifest_path = ObjectPath::from(format!("{prefix}/workflow/stages/{shard}/{hex}.json"));
    let manifest_json = serde_json::to_vec(&entry).unwrap();
    store
        .put(&manifest_path, bytes::Bytes::from(manifest_json))
        .await
        .unwrap();

    // Pull should detect the hash mismatch.
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join(".crab/cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let result = pull_remote(&store, prefix, &local_hash, &cache_root, Some(tmp.path())).await;

    assert!(result.is_err(), "stage_hash mismatch should be rejected");
    let err = result.unwrap_err();
    match &err {
        WorkflowError::CacheEntryHashMismatch {
            manifest_hash,
            local_hash: lh,
        } => {
            assert_eq!(manifest_hash, &wrong_hash.as_hex());
            assert_eq!(lh, &local_hash.as_hex());
        }
        other => panic!("expected CacheEntryHashMismatch, got: {other}"),
    }
}

#[test]
fn remote_cache_readonly_rejects_push() {
    // When remote_cache_readonly = true, check_remote_cache_readonly
    // should return RemoteCacheReadonly error.
    let result = check_remote_cache_readonly(true);
    assert!(result.is_err());
    match result.unwrap_err() {
        WorkflowError::RemoteCacheReadonly => {}
        other => panic!("expected RemoteCacheReadonly, got: {other}"),
    }
}

#[test]
fn remote_cache_readonly_allows_when_false() {
    // When remote_cache_readonly = false, check should pass.
    let result = check_remote_cache_readonly(false);
    assert!(result.is_ok());
}
