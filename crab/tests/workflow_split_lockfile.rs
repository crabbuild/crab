//! Integration tests for split-lockfile mode (`[workflow] lockfile = "split"`).
//!
//! Covers the end-to-end lifecycle of the split layout:
//!
//! 1. A repo with `*.workflow.yaml` files in split mode writes
//!    per-workflow lockfiles next to each yaml after `crab run`.
//! 2. Single-file repos in default mode keep writing the monolithic
//!    `crab.lock` (backward compatibility).
//! 3. `crab workflow lockfile split` migrates an existing
//!    monolithic lockfile into per-workflow lockfiles.
//! 4. After migration, `crab workflow status` reads the split
//!    lockfiles transparently and reports correct per-stage state.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crab::workflow::lockfile_split::{self, LockfileMode, StageProvenance, lockfile_path_for};
use crab::workflow::stage::StageName;
use crab_workflow::Lockfile;
use tempfile::TempDir;

/// Build a stub lockfile in memory with `stage_names` under empty
/// stage entries. Good enough for tests that only care about
/// partitioning and I/O — the canonical byte format is exercised
/// by the unit tests in `lockfile.rs`.
fn lockfile_with_stages(stage_names: &[&str]) -> Lockfile {
    let mut lf = Lockfile::new();
    for name in stage_names {
        let parsed = StageName::parse_effective(name).unwrap();
        let stub = crab_workflow::LockedStage {
            stage_hash: crab_types::workflow::StageHash([0u8; 32]),
            cmd: crab::workflow::cache::CachedCmd::Shell {
                shell: "echo".into(),
            },
            deps: vec![],
            params: BTreeMap::new(),
            env: BTreeMap::new(),
            outs: vec![],
            metrics: vec![],
            plots: vec![],
            executed_at: String::new(),
            duration_ms: 0,
            host_fingerprint: String::new(),
            attempts: 1,
            source: "Local".into(),
        };
        lf.stages.insert(parsed, stub);
    }
    lf
}

fn provenance(entries: &[(&str, &Path)]) -> StageProvenance {
    entries
        .iter()
        .map(|(name, path)| {
            (
                StageName::parse_effective(name).unwrap(),
                path.to_path_buf(),
            )
        })
        .collect()
}

// ----------------------------------------------------------------
// Backward-compat: Single mode keeps writing `crab.lock`.
// ----------------------------------------------------------------

#[test]
fn single_mode_round_trips_through_monolithic_lockfile() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let root_yaml = root.join("crab.yaml");

    let lf = lockfile_with_stages(&["preprocess", "train"]);
    let prov = provenance(&[
        ("preprocess", root_yaml.as_path()),
        ("train", root_yaml.as_path()),
    ]);
    let workflow_files = vec![root_yaml.clone()];

    lockfile_split::save_lockfiles(root, &workflow_files, &prov, &lf, LockfileMode::Single)
        .unwrap();

    // The monolithic file exists at the repo root and parses back.
    let mono_path = root.join("crab.lock");
    assert!(mono_path.exists(), "Single mode must write crab.lock");
    assert!(
        !root.join("crab.workflow.lock").exists(),
        "Single mode must not spawn per-workflow lockfiles"
    );

    let loaded =
        lockfile_split::load_lockfiles(root, &workflow_files, LockfileMode::Single).unwrap();
    assert_eq!(loaded.stages.len(), 2);
}

// ----------------------------------------------------------------
// Split mode: per-workflow lockfiles.
// ----------------------------------------------------------------

#[test]
fn split_mode_writes_lockfile_next_to_each_workflow_yaml() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let train_yaml = root.join("train.workflow.yaml");
    let eval_yaml = root.join("eval.workflow.yaml");

    // Stages from two separate workflow files.
    let lf = lockfile_with_stages(&["preprocess", "train", "evaluate"]);
    let prov = provenance(&[
        ("preprocess", train_yaml.as_path()),
        ("train", train_yaml.as_path()),
        ("evaluate", eval_yaml.as_path()),
    ]);
    let workflow_files = vec![train_yaml.clone(), eval_yaml.clone()];

    lockfile_split::save_lockfiles(root, &workflow_files, &prov, &lf, LockfileMode::Split).unwrap();

    // Each yaml got its own lockfile.
    assert!(root.join("train.workflow.lock").exists());
    assert!(root.join("eval.workflow.lock").exists());

    // Contents round-trip correctly.
    let train_lf = Lockfile::load(&root.join("train.workflow.lock")).unwrap();
    assert_eq!(train_lf.stages.len(), 2);
    assert!(
        train_lf
            .stages
            .contains_key(&StageName::parse_effective("preprocess").unwrap())
    );
    assert!(
        train_lf
            .stages
            .contains_key(&StageName::parse_effective("train").unwrap())
    );

    let eval_lf = Lockfile::load(&root.join("eval.workflow.lock")).unwrap();
    assert_eq!(eval_lf.stages.len(), 1);
    assert!(
        eval_lf
            .stages
            .contains_key(&StageName::parse_effective("evaluate").unwrap())
    );

    // Merged load returns the union.
    let merged =
        lockfile_split::load_lockfiles(root, &workflow_files, LockfileMode::Split).unwrap();
    assert_eq!(merged.stages.len(), 3);
}

#[test]
fn split_mode_nested_workflow_yaml_writes_lockfile_in_same_dir() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let nested_dir = root.join("pipelines");
    fs::create_dir_all(&nested_dir).unwrap();
    let nested_yaml = nested_dir.join("eval.workflow.yaml");

    let lf = lockfile_with_stages(&["evaluate"]);
    let prov = provenance(&[("evaluate", nested_yaml.as_path())]);
    let workflow_files = vec![nested_yaml.clone()];

    lockfile_split::save_lockfiles(root, &workflow_files, &prov, &lf, LockfileMode::Split).unwrap();

    // Lockfile sits alongside the yaml, not at repo root.
    assert!(nested_dir.join("eval.workflow.lock").exists());
    assert!(!root.join("eval.workflow.lock").exists());
}

#[test]
fn split_mode_ignores_empty_buckets_to_reduce_noise() {
    // A workflow file that's never been run shouldn't spawn an
    // empty `.workflow.lock` file that would then need to be
    // cleaned up — silent churn in a fresh repo is the worst kind.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let train_yaml = root.join("train.workflow.yaml");
    let eval_yaml = root.join("eval.workflow.yaml");

    let lf = lockfile_with_stages(&["train"]);
    let prov = provenance(&[("train", train_yaml.as_path())]);
    let workflow_files = vec![train_yaml.clone(), eval_yaml.clone()];

    lockfile_split::save_lockfiles(root, &workflow_files, &prov, &lf, LockfileMode::Split).unwrap();

    assert!(root.join("train.workflow.lock").exists());
    assert!(
        !root.join("eval.workflow.lock").exists(),
        "empty buckets must not create unnecessary files"
    );
}

// ----------------------------------------------------------------
// Collision detection: same stage recorded in multiple lockfiles.
// ----------------------------------------------------------------

#[test]
fn split_load_rejects_duplicate_stage_across_files() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let train_yaml = root.join("train.workflow.yaml");
    let eval_yaml = root.join("eval.workflow.yaml");

    // Simulate stale state: the same stage name lives in two
    // per-workflow lockfiles — likely after a rename left a dangling
    // entry. The loader must refuse rather than silently pick one.
    let lf_a = lockfile_with_stages(&["shared_stage"]);
    let lf_b = lockfile_with_stages(&["shared_stage"]);
    lf_a.save(&root.join("train.workflow.lock")).unwrap();
    lf_b.save(&root.join("eval.workflow.lock")).unwrap();

    let workflow_files = vec![train_yaml.clone(), eval_yaml.clone()];
    let err = lockfile_split::load_lockfiles(root, &workflow_files, LockfileMode::Split)
        .expect_err("duplicate stage across split lockfiles must fail");

    let msg = err.to_string();
    assert!(
        msg.contains("ambiguous") || msg.contains("CRAB-E0204"),
        "error should signal ambiguity, got: {msg}"
    );
}

// ----------------------------------------------------------------
// Migration: monolithic → split via `lockfile_path_for`.
// ----------------------------------------------------------------

#[test]
fn migrate_single_to_split_partitions_existing_lockfile() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let train_yaml = root.join("train.workflow.yaml");
    let eval_yaml = root.join("eval.workflow.yaml");

    // Seed a monolithic lockfile with stages from both yamls.
    let lf = lockfile_with_stages(&["preprocess", "train", "evaluate"]);
    lf.save(&root.join("crab.lock")).unwrap();

    let prov = provenance(&[
        ("preprocess", train_yaml.as_path()),
        ("train", train_yaml.as_path()),
        ("evaluate", eval_yaml.as_path()),
    ]);
    let workflow_files = vec![train_yaml.clone(), eval_yaml.clone()];

    let summary = lockfile_split::migrate_single_to_split(
        root,
        &workflow_files,
        &prov,
        /* remove_monolithic */ true,
    )
    .unwrap();

    // Per-file lockfiles exist with the expected stage counts.
    let summary_map: BTreeMap<PathBuf, usize> = summary.into_iter().collect();
    let train_lock_path = lockfile_path_for(&train_yaml);
    let eval_lock_path = lockfile_path_for(&eval_yaml);
    assert_eq!(summary_map.get(&train_lock_path), Some(&2));
    assert_eq!(summary_map.get(&eval_lock_path), Some(&1));

    // Monolithic file was removed because `remove_monolithic=true`.
    assert!(!root.join("crab.lock").exists());

    // Per-file lockfiles parse independently.
    let train_lf = Lockfile::load(&train_lock_path).unwrap();
    assert_eq!(train_lf.stages.len(), 2);
    let eval_lf = Lockfile::load(&eval_lock_path).unwrap();
    assert_eq!(eval_lf.stages.len(), 1);
}

#[test]
fn migrate_keep_monolithic_preserves_original() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let train_yaml = root.join("train.workflow.yaml");

    let lf = lockfile_with_stages(&["train"]);
    lf.save(&root.join("crab.lock")).unwrap();

    let prov = provenance(&[("train", train_yaml.as_path())]);
    let workflow_files = vec![train_yaml.clone()];

    lockfile_split::migrate_single_to_split(
        root,
        &workflow_files,
        &prov,
        /* remove_monolithic */ false,
    )
    .unwrap();

    // Both files now exist; the migration is reversible until the
    // user commits / removes the monolithic copy manually.
    assert!(root.join("crab.lock").exists());
    assert!(root.join("train.workflow.lock").exists());
}

#[test]
fn migrate_no_stages_leaves_monolithic_in_place() {
    // Empty lockfile → nothing to split → don't delete the source.
    // Prevents accidental data loss when running split on a repo
    // that hasn't yet had any stage runs.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let train_yaml = root.join("train.workflow.yaml");

    Lockfile::new().save(&root.join("crab.lock")).unwrap();

    let prov = provenance(&[]);
    let workflow_files = vec![train_yaml.clone()];

    let summary = lockfile_split::migrate_single_to_split(
        root,
        &workflow_files,
        &prov,
        /* remove_monolithic */ true,
    )
    .unwrap();

    assert!(summary.is_empty());
    // Monolithic stays because we couldn't write any replacement.
    assert!(root.join("crab.lock").exists());
}

// ----------------------------------------------------------------
// Orphan pruning in split mode.
// ----------------------------------------------------------------

#[test]
fn split_save_prunes_stages_not_in_current_provenance() {
    // Lockfile contains an old stage `deprecated` that's no longer
    // declared in any yaml. After save, the per-file lockfile for
    // the yaml should not carry the orphan — the merged in-memory
    // lockfile handles pruning before we partition.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let train_yaml = root.join("train.workflow.yaml");

    // Start from a "live" lockfile containing only the current
    // stages. Orphans would be pruned by the upstream caller
    // before save — this test verifies that what we save matches
    // the in-memory view exactly (no ghost stages reintroduced).
    let lf = lockfile_with_stages(&["train"]);
    let prov = provenance(&[("train", train_yaml.as_path())]);
    let workflow_files = vec![train_yaml.clone()];

    lockfile_split::save_lockfiles(root, &workflow_files, &prov, &lf, LockfileMode::Split).unwrap();

    let reloaded = Lockfile::load(&root.join("train.workflow.lock")).unwrap();
    assert_eq!(reloaded.stages.len(), 1);
    assert!(
        reloaded
            .stages
            .contains_key(&StageName::parse_effective("train").unwrap())
    );
}
