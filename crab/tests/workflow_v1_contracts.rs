//! Strict canonical-v1 workflow format integration tests.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use std::path::Path;

use crab::workflow::WorkflowError;
use crab::workflow::cache::{
    CachedCmd, CachedOut, ENTRY_SCHEMA_MAX_SUPPORTED, ENTRY_SCHEMA_VERSION, StageCacheEntry,
    entry_path, read_local, write_local,
};
use crab::workflow::stage::OutKind;
use crab_types::workflow::StageHash;
use crab_workflow::{LOCKFILE_HASH_ALGO, LOCKFILE_SCHEMA_VERSION, LockedStage, Lockfile};
use tempfile::TempDir;

fn cache_entry(hash: StageHash) -> StageCacheEntry {
    StageCacheEntry {
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
        host_fingerprint: "darwin-aarch64-crab-1".to_owned(),
    }
}

#[test]
fn canonical_v1_cache_entry_roundtrips() {
    let tmp = TempDir::new().unwrap();
    let hash = StageHash([0xee; 32]);
    let entry = cache_entry(hash);

    write_local(tmp.path(), &entry).unwrap();

    assert_eq!(read_local(tmp.path(), &hash).unwrap(), Some(entry));
}

#[test]
fn non_v1_cache_entry_is_refused_without_rewrite() {
    let tmp = TempDir::new().unwrap();
    let hash = StageHash([0xdd; 32]);
    let path = entry_path(tmp.path(), &hash);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut value = serde_json::to_value(cache_entry(hash)).unwrap();
    value["schema_version"] = serde_json::json!(2);
    let bytes = serde_json::to_vec(&value).unwrap();
    std::fs::write(&path, &bytes).unwrap();

    let error = read_local(tmp.path(), &hash).unwrap_err();

    assert!(matches!(
        error,
        WorkflowError::CacheEntrySchemaNewer {
            found: 2,
            supported: 1,
            ..
        }
    ));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn canonical_v1_lockfile_roundtrips() {
    let mut lockfile = Lockfile::new();
    let name = crab::workflow::stage::StageName::parse("deploy").unwrap();
    lockfile.stages.insert(
        name.clone(),
        LockedStage {
            stage_hash: StageHash([0x55; 32]),
            cmd: CachedCmd::Shell {
                shell: "make deploy".into(),
            },
            deps: vec![],
            params: Default::default(),
            env: Default::default(),
            outs: vec![],
            metrics: vec![],
            plots: vec![],
            executed_at: "2026-07-15T08:00:00.000Z".into(),
            duration_ms: 1500,
            host_fingerprint: "linux-x86_64-crab-1".into(),
            attempts: 1,
            source: "Remote".into(),
        },
    );

    let bytes = lockfile.serialize_canonical().unwrap();
    let parsed = Lockfile::parse(Path::new("crab.lock"), &bytes).unwrap();

    assert_eq!(parsed, lockfile);
    assert_eq!(parsed.get(&name).unwrap().source, "Remote");
}

#[test]
fn non_v1_and_incomplete_lockfiles_are_rejected() {
    let non_v1 = b"crab_hash_algo: \"crab.stage.v1\"\nschema_version: 2\nstages: {}\n";
    assert!(Lockfile::parse(Path::new("crab.lock"), non_v1).is_err());

    let missing_current_fields = b"crab_hash_algo: \"crab.stage.v1\"\nschema_version: 1\nstages:\n  train:\n    cmd:\n      shell: \"echo x\"\n";
    assert!(Lockfile::parse(Path::new("crab.lock"), missing_current_fields).is_err());
}

#[test]
fn workflow_contract_inventory_is_v1() {
    assert_eq!(ENTRY_SCHEMA_VERSION, 1);
    assert_eq!(ENTRY_SCHEMA_MAX_SUPPORTED, 1);
    assert_eq!(LOCKFILE_SCHEMA_VERSION, 1);
    assert_eq!(LOCKFILE_HASH_ALGO, "crab.stage.v1");
}
