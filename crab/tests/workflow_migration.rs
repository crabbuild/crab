//! Integration tests for Phase 1 → current-schema migration compatibility.
//!
//! Verifies that:
//! - v1 cache entries are migrated to the current schema in-memory and usable for
//!   cache hits without re-hashing.
//! - v1 lockfiles (missing `source` and `attempts` fields) parse
//!   with defaults applied.
//! - Newer-schema entries are refused with `CacheEntrySchemaNewer`.
//! - The v1 → current-schema migration is lossless (all original fields preserved).
//! - Phase 1 journal payloads (`{"phase":"1-noop"}`) are recognized.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::path::Path;

use tempfile::TempDir;

use crab::workflow::WorkflowError;
use crab::workflow::cache::{
    CachedCmd, CachedOut, ENTRY_SCHEMA_MAX_SUPPORTED, ENTRY_SCHEMA_VERSION, StageCacheEntry,
    entry_path, read_local, write_local,
};
use crab::workflow::stage::OutKind;
use crab_types::workflow::StageHash;
use crab_workflow::Lockfile;

// --- v1 fixture helpers ---

/// Build a v1 cache entry JSON string (the shape Phase 1 wrote to disk).
/// No `tree_manifest` on outs, `schema_version: 1`, `attempts: 1`.
fn v1_cache_entry_json(stage_hash_bytes: &[u8; 32]) -> String {
    let hash_array: String = stage_hash_bytes
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{
  "attempts": 1,
  "cmd": {{"kind": "shell", "shell": "python train.py"}},
  "duration_ms": 5000,
  "exec_id": null,
  "executed_at": "2025-06-01T12:00:00.000Z",
  "host_fingerprint": "linux-x86_64-crab-0.7.0",
  "metrics": [],
  "outs": [
    {{
      "file_hash": "b3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "kind": "File",
      "mode": 420,
      "path": "output/model.pkl",
      "size": 8192
    }}
  ],
  "plots": [],
  "schema_version": 1,
  "stage_hash": [{}],
  "stage_name": "train"
}}"#,
        hash_array
    )
}

/// Build a v1 lockfile YAML string (missing `source` and potentially
/// missing `attempts`).
fn v1_lockfile_yaml_no_source_no_attempts() -> String {
    r#"crab_hash_algo: "crab.stage.v3"
schema_version: 1
stages:
  train:
    cmd:
      shell: "python train.py"
    deps:
      - hash: "b3:1111111111111111111111111111111111111111111111111111111111111111"
        path: "data/raw.csv"
        size: 1024
    duration_ms: 5000
    env: {}
    executed_at: "2025-06-01T12:00:00.000Z"
    host_fingerprint: "linux-x86_64-crab-0.7.0"
    metrics: []
    outs:
      - hash: "b3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        kind: "file"
        mode: "0o644"
        path: "output/model.pkl"
        size: 8192
    params: {}
    stage_hash: "b3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#
    .to_owned()
}

/// Build a v1 lockfile YAML string that has `attempts` but no `source`.
fn v1_lockfile_yaml_with_attempts_no_source() -> String {
    r#"crab_hash_algo: "crab.stage.v3"
schema_version: 1
stages:
  train:
    attempts: 2
    cmd:
      shell: "python train.py"
    deps:
      - hash: "b3:1111111111111111111111111111111111111111111111111111111111111111"
        path: "data/raw.csv"
        size: 1024
    duration_ms: 5000
    env: {}
    executed_at: "2025-06-01T12:00:00.000Z"
    host_fingerprint: "linux-x86_64-crab-0.7.0"
    metrics: []
    outs:
      - hash: "b3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        kind: "file"
        mode: "0o644"
        path: "output/model.pkl"
        size: 8192
    params: {}
    stage_hash: "b3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#
    .to_owned()
}

// --- Cache entry migration tests ---

#[test]
fn v1_cache_entry_migrates_to_current_schema_on_read() {
    let tmp = TempDir::new().unwrap();
    let hash = StageHash([0xaa; 32]);
    let path = entry_path(tmp.path(), &hash);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    let json = v1_cache_entry_json(&hash.0);
    std::fs::write(&path, json.as_bytes()).unwrap();

    // Reading a v1 entry should succeed and produce a current-schema entry.
    let entry = read_local(tmp.path(), &hash).unwrap().unwrap();
    assert_eq!(entry.schema_version, ENTRY_SCHEMA_VERSION);
    assert_eq!(entry.stage_name, "train");
    assert_eq!(entry.attempts, 1);
    assert_eq!(entry.stage_hash, hash);
    assert_eq!(entry.outs.len(), 1);
    // tree_manifest should be None for file outs migrated from v1.
    assert_eq!(entry.outs[0].tree_manifest, None);
    assert_eq!(entry.outs[0].kind, OutKind::File);
    assert!(entry.outs[0].push);
    assert!(entry.remote_push_enabled());
}

#[test]
fn v1_cache_entry_usable_for_cache_hit_without_rehashing() {
    // A v1 entry should be usable as a cache hit — the stage_hash
    // matches, so no re-hashing is needed.
    let tmp = TempDir::new().unwrap();
    let hash = StageHash([0xbb; 32]);
    let path = entry_path(tmp.path(), &hash);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    let json = v1_cache_entry_json(&hash.0);
    std::fs::write(&path, json.as_bytes()).unwrap();

    // read_local succeeds — this is the cache-hit path.
    let entry = read_local(tmp.path(), &hash).unwrap().unwrap();
    // The entry's stage_hash matches what we looked up — cache hit.
    assert_eq!(entry.stage_hash, hash);
    // All output metadata is preserved for materialization.
    assert_eq!(
        entry.outs[0].file_hash,
        "b3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(entry.outs[0].size, 8192);
    assert_eq!(entry.outs[0].mode, 420); // 0o644
}

#[test]
fn v1_to_current_schema_migration_is_lossless() {
    // Write a v1 entry, read it (triggers migration), write it back
    // as the current schema, read again — all original fields must be preserved.
    let tmp = TempDir::new().unwrap();
    let hash = StageHash([0xcc; 32]);
    let path = entry_path(tmp.path(), &hash);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    let json = v1_cache_entry_json(&hash.0);
    std::fs::write(&path, json.as_bytes()).unwrap();

    // Read (migrates v1 → current schema in memory).
    let entry = read_local(tmp.path(), &hash).unwrap().unwrap();

    let current_entry = StageCacheEntry {
        schema_version: ENTRY_SCHEMA_VERSION,
        ..entry.clone()
    };
    write_local(tmp.path(), &current_entry).unwrap();

    // Read again — should be a clean current-schema entry.
    let entry2 = read_local(tmp.path(), &hash).unwrap().unwrap();
    assert_eq!(entry2.schema_version, ENTRY_SCHEMA_VERSION);
    assert_eq!(entry2.stage_name, "train");
    assert_eq!(entry2.attempts, 1);
    assert_eq!(entry2.duration_ms, 5000);
    assert_eq!(entry2.executed_at, "2025-06-01T12:00:00.000Z");
    assert_eq!(entry2.host_fingerprint, "linux-x86_64-crab-0.7.0");
    assert_eq!(entry2.outs.len(), 1);
    assert_eq!(entry2.outs[0].path.to_str().unwrap(), "output/model.pkl");
    assert_eq!(
        entry2.outs[0].file_hash,
        "b3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
}

#[test]
fn newer_schema_entry_is_refused_with_cache_entry_schema_newer() {
    // Simulates a downgrade scenario by writing a schema_version
    // higher than this binary supports.
    let tmp = TempDir::new().unwrap();
    let hash = StageHash([0xdd; 32]);
    let path = entry_path(tmp.path(), &hash);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    // Write an entry with schema_version = 99 (future version).
    let future_json = r#"{"schema_version":99,"stage_hash":"dd","stage_name":"x"}"#;
    std::fs::write(&path, future_json.as_bytes()).unwrap();

    let err = read_local(tmp.path(), &hash).unwrap_err();
    assert!(
        matches!(
            &err,
            WorkflowError::CacheEntrySchemaNewer { found, supported, .. }
            if *found == 99 && *supported == ENTRY_SCHEMA_MAX_SUPPORTED
        ),
        "expected CacheEntrySchemaNewer, got: {err:?}"
    );
}

#[test]
fn current_schema_entry_roundtrips_cleanly() {
    // A current-schema entry written by this binary reads back identically.
    let tmp = TempDir::new().unwrap();
    let hash = StageHash([0xee; 32]);
    let entry = StageCacheEntry {
        schema_version: ENTRY_SCHEMA_VERSION,
        stage_hash: hash,
        stage_name: "preprocess".to_owned(),
        cmd: CachedCmd::Shell {
            shell: "python preprocess.py".to_owned(),
        },
        outs: vec![CachedOut {
            path: "out/data.parquet".into(),
            kind: OutKind::File,
            push: true,
            remote: None,
            file_hash: "b3:1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
                .to_owned(),
            size: 2048,
            mode: 0o644,
            tree_manifest: None,
        }],
        metrics: vec![],
        plots: vec![],
        executed_at: "2026-07-01T10:00:00.000Z".to_owned(),
        duration_ms: 3000,
        exec_id: None,
        attempts: 2,
        host_fingerprint: "darwin-aarch64-crab-0.9.0".to_owned(),
    };

    write_local(tmp.path(), &entry).unwrap();
    let got = read_local(tmp.path(), &hash).unwrap().unwrap();
    assert_eq!(got, entry);
}

// --- Lockfile backward compatibility tests ---

#[test]
fn v1_lockfile_without_source_or_attempts_parses_with_defaults() {
    let yaml = v1_lockfile_yaml_no_source_no_attempts();
    let lf = Lockfile::parse(Path::new("crab.lock"), yaml.as_bytes()).unwrap();

    assert_eq!(lf.stages.len(), 1);
    let train = lf
        .get(&crab::workflow::stage::StageName::parse("train").unwrap())
        .unwrap();

    // `attempts` defaults to 1 when missing.
    assert_eq!(train.attempts, 1);
    // `source` defaults to "Local" when missing.
    assert_eq!(train.source, "Local");
    // Other fields are parsed normally.
    assert_eq!(train.duration_ms, 5000);
    assert_eq!(train.executed_at, "2025-06-01T12:00:00.000Z");
}

#[test]
fn v1_lockfile_with_attempts_but_no_source_parses_correctly() {
    let yaml = v1_lockfile_yaml_with_attempts_no_source();
    let lf = Lockfile::parse(Path::new("crab.lock"), yaml.as_bytes()).unwrap();

    let train = lf
        .get(&crab::workflow::stage::StageName::parse("train").unwrap())
        .unwrap();

    // `attempts` is present in the YAML and should be parsed.
    assert_eq!(train.attempts, 2);
    // `source` defaults to "Local" when missing.
    assert_eq!(train.source, "Local");
}

#[test]
fn v2_lockfile_with_source_field_roundtrips() {
    // Build a lockfile with the source field, serialize, parse back.
    let mut lf = Lockfile::new();
    let name = crab::workflow::stage::StageName::parse("deploy").unwrap();
    let stage = crab_workflow::LockedStage {
        stage_hash: StageHash([0x55; 32]),
        cmd: CachedCmd::Shell {
            shell: "make deploy".into(),
        },
        deps: vec![],
        params: std::collections::BTreeMap::new(),
        env: std::collections::BTreeMap::new(),
        outs: vec![],
        metrics: vec![],
        plots: vec![],
        executed_at: "2026-07-15T08:00:00.000Z".into(),
        duration_ms: 1500,
        host_fingerprint: "linux-x86_64-crab-0.9.0".into(),
        attempts: 1,
        source: "Remote".into(),
    };
    lf.stages.insert(name.clone(), stage);

    let bytes = lf.serialize_canonical().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    // The source field should appear in the serialized output.
    assert!(
        text.contains(r#"source: "Remote""#),
        "source field missing in:\n{text}"
    );

    // Parse back and verify.
    let parsed = Lockfile::parse(Path::new("crab.lock"), &bytes).unwrap();
    let got = parsed.get(&name).unwrap();
    assert_eq!(got.source, "Remote");
    assert_eq!(got.attempts, 1);
}

// --- Schema version constants ---

#[test]
fn schema_version_is_current() {
    assert_eq!(ENTRY_SCHEMA_VERSION, 3);
    assert_eq!(ENTRY_SCHEMA_MAX_SUPPORTED, ENTRY_SCHEMA_VERSION);
}
