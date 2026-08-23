//! Performance budget scaffolding for the Storage Economy subsystem
//! and the Workflow Engine subsystem.
//!
//! Every test is `#[ignore]`-guarded so it never runs in normal CI.
//! Run manually under release mode:
//!
//! ```sh
//! cargo test -p crab --test perf_budgets -- --ignored --release
//! ```
//!
//! The TOML file at `tests/perf_budgets/storage_economy.toml` is the
//! single source of truth for budget numbers. Actual benchmark
//! implementations will be added in later tasks; these placeholders
//! parse the budget and print the target.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use serde::Deserialize;

/// Path to the budget TOML relative to the crate root.
const BUDGET_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/perf_budgets/storage_economy.toml"
);

// ---------------------------------------------------------------------------
// Budget types — mirrors the TOML structure
// ---------------------------------------------------------------------------

/// Top-level budget file parsed as a flat map of named sections.
/// Each section is a [`BudgetEntry`] with heterogeneous optional fields.
#[derive(Debug, Deserialize)]
struct BudgetFile {
    tier_plan: TimeBudgetMs,
    tier_plan_apply_s3: TimeBudgetMs,
    hydrate_post_restore: OverheadBudget,
    doctor_cost_live: ScaleBudgetSecs,
    doctor_cost_report: ScaleBudgetSecs,
    optimize_xorbs_throughput: ThroughputBudget,
}

#[derive(Debug, Deserialize)]
struct TimeBudgetMs {
    max_ms: u64,
    description: String,
}

#[derive(Debug, Deserialize)]
struct OverheadBudget {
    max_overhead_percent: f64,
    description: String,
}

#[derive(Debug, Deserialize)]
struct ScaleBudgetSecs {
    max_secs: u64,
    #[serde(default)]
    concurrency: Option<u32>,
    #[serde(default)]
    objects: Option<u64>,
    description: String,
}

#[derive(Debug, Deserialize)]
struct ThroughputBudget {
    fixture_gib: u64,
    description: String,
}

/// Load and parse the budget file. Panics on missing/malformed TOML so
/// test failures are immediately obvious.
fn load_budgets() -> BudgetFile {
    let raw = std::fs::read_to_string(BUDGET_PATH)
        .unwrap_or_else(|e| panic!("failed to read budget TOML at {BUDGET_PATH}: {e}"));
    toml::from_str(&raw).unwrap_or_else(|e| panic!("failed to parse budget TOML: {e}"))
}

// ---------------------------------------------------------------------------
// Placeholder benchmarks — one per budget entry
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn perf_tier_plan() {
    let b = load_budgets();
    eprintln!(
        "[perf] tier_plan: target < {} ms — {}",
        b.tier_plan.max_ms, b.tier_plan.description,
    );
    // Actual benchmark will be wired in a later task.
}

#[test]
#[ignore]
fn perf_tier_plan_apply_s3() {
    let b = load_budgets();
    eprintln!(
        "[perf] tier_plan_apply_s3: target < {} ms — {}",
        b.tier_plan_apply_s3.max_ms, b.tier_plan_apply_s3.description,
    );
}

#[test]
#[ignore]
fn perf_hydrate_post_restore() {
    let b = load_budgets();
    eprintln!(
        "[perf] hydrate_post_restore: target < {:.1}% overhead — {}",
        b.hydrate_post_restore.max_overhead_percent, b.hydrate_post_restore.description,
    );
}

#[test]
#[ignore]
fn perf_doctor_cost_live() {
    let b = load_budgets();
    eprintln!(
        "[perf] doctor_cost_live: target < {} s, concurrency={}, objects={} — {}",
        b.doctor_cost_live.max_secs,
        b.doctor_cost_live.concurrency.unwrap_or(0),
        b.doctor_cost_live.objects.unwrap_or(0),
        b.doctor_cost_live.description,
    );
}

#[test]
#[ignore]
fn perf_doctor_cost_report() {
    let b = load_budgets();
    eprintln!(
        "[perf] doctor_cost_report: target < {} s, objects={} — {}",
        b.doctor_cost_report.max_secs,
        b.doctor_cost_report.objects.unwrap_or(0),
        b.doctor_cost_report.description,
    );
}

#[test]
#[ignore]
fn perf_optimize_xorbs_throughput() {
    let b = load_budgets();
    eprintln!(
        "[perf] optimize_xorbs_throughput: fixture {} GiB, target >= push throughput — {}",
        b.optimize_xorbs_throughput.fixture_gib, b.optimize_xorbs_throughput.description,
    );
}

// ---------------------------------------------------------------------------
// Non-ignored smoke test: TOML parses correctly
// ---------------------------------------------------------------------------

/// This test is NOT ignored — it runs in normal CI to catch TOML drift.
#[test]
fn budget_toml_parses() {
    let b = load_budgets();
    assert!(b.tier_plan.max_ms > 0);
    assert!(b.tier_plan_apply_s3.max_ms > 0);
    assert!(b.hydrate_post_restore.max_overhead_percent > 0.0);
    assert!(b.doctor_cost_live.max_secs > 0);
    assert!(b.doctor_cost_report.max_secs > 0);
    assert!(b.optimize_xorbs_throughput.fixture_gib > 0);
}

// ---------------------------------------------------------------------------
// Workflow Engine performance budgets
// ---------------------------------------------------------------------------
//
// These tests assert the performance budgets from the workflow engine
// design document. They use 2x margin over the design targets to
// account for CI machine variability:
//   - Local cache hit: < 100ms (design: < 50ms)
//   - Lockfile write (100 stages): < 100ms (design: < 50ms)
//   - Explain-miss (100 stages): < 400ms (design: < 200ms)

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::TempDir;

use crab::workflow::cache::{
    CachedCmd, CachedOut, ENTRY_SCHEMA_VERSION, StageCacheEntry, read_local, write_local,
};
use crab::workflow::stage::{OutKind, StageName};
use crab_types::workflow::StageHash;
use crab_workflow::{LockedDep, LockedOut, LockedStage, Lockfile};

/// Budget: local cache hit for a single-file stage must complete in < 100ms.
/// (Design target is 50ms; we use 2x margin for CI variability.)
const WORKFLOW_CACHE_HIT_MAX_MS: u128 = 100;

/// Budget: lockfile write for 100 stages must complete in < 100ms.
/// (Design target is 50ms; we use 2x margin for CI variability.)
const WORKFLOW_LOCKFILE_WRITE_MAX_MS: u128 = 100;

/// Budget: explain-miss computation for 100 stages must complete in < 400ms.
/// (Design target is 200ms; we use 2x margin for CI variability.)
const WORKFLOW_EXPLAIN_MISS_MAX_MS: u128 = 400;

/// Generate a deterministic stage hash from an index.
fn make_stage_hash(idx: u32) -> StageHash {
    let mut bytes = [0u8; 32];
    bytes[0..4].copy_from_slice(&idx.to_le_bytes());
    // Fill remaining bytes with a pattern for uniqueness
    for i in 4..32 {
        bytes[i] = ((idx as u8).wrapping_add(i as u8)) ^ 0xAB;
    }
    StageHash(bytes)
}

/// Build a sample StageCacheEntry for benchmarking.
fn make_cache_entry(idx: u32) -> StageCacheEntry {
    StageCacheEntry {
        schema_version: ENTRY_SCHEMA_VERSION,
        stage_hash: make_stage_hash(idx),
        stage_name: format!("stage_{idx:03}"),
        cmd: CachedCmd::Shell {
            shell: format!("python train.py --seed {idx}"),
        },
        outs: vec![CachedOut {
            path: PathBuf::from(format!("output/model_{idx:03}.bin")),
            kind: OutKind::File,
            push: true,
            remote: None,
            file_hash: format!("b3:{:064x}", (idx as u128) * 0x1234_5678_9ABC_DEF0),
            size: 1024 * (idx as u64 + 1),
            mode: 0o644,
            tree_manifest: None,
        }],
        metrics: vec![],
        plots: vec![],
        executed_at: "2025-01-15T10:30:00.000Z".to_owned(),
        duration_ms: 5000 + (idx as u64 * 100),
        exec_id: None,
        attempts: 1,
        host_fingerprint: "test-host-abc123".to_owned(),
    }
}

/// Build a LockedStage with realistic dep/param/env data for benchmarking.
fn make_locked_stage(idx: u32) -> LockedStage {
    let mut deps = Vec::new();
    // Each stage has 5 deps to simulate a realistic workload
    for d in 0u32..5 {
        let mut hash = [0u8; 32];
        hash[0..4].copy_from_slice(&idx.to_le_bytes());
        hash[4..8].copy_from_slice(&d.to_le_bytes());
        deps.push(LockedDep {
            path: PathBuf::from(format!("data/input_{idx:03}_{d}.csv")),
            hash,
            size: 4096 * (d as u64 + 1),
        });
    }

    let mut params = BTreeMap::new();
    params.insert("model.lr".to_owned(), format!("0.00{idx}"));
    params.insert("model.epochs".to_owned(), format!("{}", 10 + idx));
    params.insert("model.batch_size".to_owned(), "32".to_owned());

    let mut env = BTreeMap::new();
    env.insert("CUDA_VISIBLE_DEVICES".to_owned(), "0".to_owned());
    env.insert("OMP_NUM_THREADS".to_owned(), "4".to_owned());

    LockedStage {
        stage_hash: make_stage_hash(idx),
        cmd: CachedCmd::Shell {
            shell: format!("python train.py --seed {idx}"),
        },
        deps,
        params,
        env,
        outs: vec![LockedOut {
            path: PathBuf::from(format!("output/model_{idx:03}.bin")),
            kind: OutKind::File,
            hash: make_stage_hash(idx).0,
            size: 1024 * (idx as u64 + 1),
            mode: 0o644,
        }],
        metrics: vec![],
        plots: vec![],
        executed_at: "2025-01-15T10:30:00.000Z".to_owned(),
        duration_ms: 5000 + (idx as u64 * 100),
        host_fingerprint: "test-host-abc123".to_owned(),
        attempts: 1,
        source: "Local".to_owned(),
    }
}

/// Local cache hit benchmark: write a single-file stage entry to the
/// local cache (miss), then read it back (hit) and assert the read
/// completes within the budget.
#[test]
#[ignore]
fn perf_workflow_cache_hit_local() {
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path();

    let entry = make_cache_entry(0);
    let hash = entry.stage_hash;

    // First access: write (simulates the miss path producing a cache entry)
    write_local(cache_root, &entry).unwrap();

    // Second access: read (simulates the hit path)
    let start = Instant::now();
    let result = read_local(cache_root, &hash).unwrap();
    let elapsed = start.elapsed();

    assert!(result.is_some(), "cache entry should exist after write");
    let elapsed_ms = elapsed.as_millis();
    eprintln!(
        "[perf] workflow_cache_hit_local: {elapsed_ms} ms (budget: < {WORKFLOW_CACHE_HIT_MAX_MS} ms)"
    );
    assert!(
        elapsed_ms < WORKFLOW_CACHE_HIT_MAX_MS,
        "local cache hit took {elapsed_ms} ms, exceeds budget of {WORKFLOW_CACHE_HIT_MAX_MS} ms"
    );
}

/// Lockfile write benchmark: build a lockfile with 100 stages and
/// measure the serialize + atomic write time.
#[test]
#[ignore]
fn perf_workflow_lockfile_write_100_stages() {
    let tmp = TempDir::new().unwrap();
    let lockfile_path = tmp.path().join("crab.lock");

    // Build a lockfile with 100 stages
    let mut lockfile = Lockfile::new();
    for idx in 0..100u32 {
        let name = StageName::parse(&format!("stage_{idx:03}")).unwrap();
        lockfile.stages.insert(name, make_locked_stage(idx));
    }

    // Measure serialize + write
    let start = Instant::now();
    lockfile.save(&lockfile_path).unwrap();
    let elapsed = start.elapsed();

    let elapsed_ms = elapsed.as_millis();
    eprintln!(
        "[perf] workflow_lockfile_write_100_stages: {elapsed_ms} ms (budget: < {WORKFLOW_LOCKFILE_WRITE_MAX_MS} ms)"
    );
    assert!(
        elapsed_ms < WORKFLOW_LOCKFILE_WRITE_MAX_MS,
        "lockfile write (100 stages) took {elapsed_ms} ms, exceeds budget of {WORKFLOW_LOCKFILE_WRITE_MAX_MS} ms"
    );

    // Sanity: file exists and is non-empty
    let meta = std::fs::metadata(&lockfile_path).unwrap();
    assert!(meta.len() > 0, "lockfile should be non-empty");
}

/// Explain-miss benchmark: set up a lockfile with 100 stages, modify
/// one dep in the "current" resolved state, and measure the
/// diff_against_resolved computation across all 100 stages.
#[test]
#[ignore]
fn perf_workflow_explain_miss_100_stages() {
    // Build a lockfile with 100 stages
    let mut lockfile = Lockfile::new();
    for idx in 0..100u32 {
        let name = StageName::parse(&format!("stage_{idx:03}")).unwrap();
        lockfile.stages.insert(name, make_locked_stage(idx));
    }

    // For each stage, build "current" resolved inputs that differ by
    // one dep hash (simulating a single file change causing a miss).
    let stage_inputs: Vec<(
        StageName,
        BTreeMap<String, [u8; 32]>,
        BTreeMap<String, String>,
        BTreeMap<String, String>,
        CachedCmd,
    )> = (0..100u32)
        .map(|idx| {
            let name = StageName::parse(&format!("stage_{idx:03}")).unwrap();

            // Current deps: same as locked, but modify the first dep's hash
            let mut dep_hashes = BTreeMap::new();
            for d in 0..5u32 {
                let mut hash = [0u8; 32];
                hash[0..4].copy_from_slice(&idx.to_le_bytes());
                hash[4..8].copy_from_slice(&d.to_le_bytes());
                if d == 0 {
                    // Flip a byte to simulate a changed dep
                    hash[31] ^= 0xFF;
                }
                dep_hashes.insert(format!("data/input_{idx:03}_{d}.csv"), hash);
            }

            let mut params = BTreeMap::new();
            params.insert("model.lr".to_owned(), format!("0.00{idx}"));
            params.insert("model.epochs".to_owned(), format!("{}", 10 + idx));
            params.insert("model.batch_size".to_owned(), "32".to_owned());

            let mut env = BTreeMap::new();
            env.insert("CUDA_VISIBLE_DEVICES".to_owned(), "0".to_owned());
            env.insert("OMP_NUM_THREADS".to_owned(), "4".to_owned());

            let cmd = CachedCmd::Shell {
                shell: format!("python train.py --seed {idx}"),
            };

            (name, dep_hashes, params, env, cmd)
        })
        .collect();

    // Measure the explain-miss computation across all 100 stages
    let start = Instant::now();
    for (name, dep_hashes, params, env, cmd) in &stage_inputs {
        let diffs = lockfile.diff_against_resolved(name, dep_hashes, params, env, cmd);
        // Each stage should produce exactly one diff (the modified dep)
        assert!(diffs.is_some(), "stage {name} should have a lockfile entry");
        let diffs = diffs.unwrap();
        assert!(
            !diffs.is_empty(),
            "stage {name} should detect the changed dep"
        );
    }
    let elapsed = start.elapsed();

    let elapsed_ms = elapsed.as_millis();
    eprintln!(
        "[perf] workflow_explain_miss_100_stages: {elapsed_ms} ms (budget: < {WORKFLOW_EXPLAIN_MISS_MAX_MS} ms)"
    );
    assert!(
        elapsed_ms < WORKFLOW_EXPLAIN_MISS_MAX_MS,
        "explain-miss (100 stages) took {elapsed_ms} ms, exceeds budget of {WORKFLOW_EXPLAIN_MISS_MAX_MS} ms"
    );
}

/// Split-lockfile write benchmark: 10 workflow files, 10 stages each
/// (100 total), partitioned into 10 per-workflow lockfiles.
///
/// Budget: under 2x the monolithic write budget. Each per-file
/// lockfile is 10x smaller than the monolithic case, but we pay
/// an atomic-write per file instead of one. On a healthy SSD the
/// overhead is dominated by the tempfile+rename round trips, so we
/// budget accordingly and alert if it regresses by more than 2x.
#[test]
#[ignore]
fn perf_workflow_lockfile_split_write_100_stages_10_files() {
    use crab::workflow::lockfile_split::{self, LockfileMode, StageProvenance};
    use std::collections::BTreeMap;

    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Build 10 workflow yaml paths and spread 100 stages across them.
    let mut workflow_files = Vec::new();
    let mut prov: StageProvenance = BTreeMap::new();
    let mut lockfile = Lockfile::new();
    for wf_idx in 0..10u32 {
        let wf_path = root.join(format!("wf_{wf_idx:02}.workflow.yaml"));
        workflow_files.push(wf_path.clone());
        for s_idx in 0..10u32 {
            let idx = wf_idx * 10 + s_idx;
            let name = StageName::parse(&format!("stage_{idx:03}")).unwrap();
            prov.insert(name.clone(), wf_path.clone());
            lockfile.stages.insert(name, make_locked_stage(idx));
        }
    }

    let start = Instant::now();
    lockfile_split::save_lockfiles(root, &workflow_files, &prov, &lockfile, LockfileMode::Split)
        .unwrap();
    let elapsed = start.elapsed();

    let elapsed_ms = elapsed.as_millis();
    // 2x the monolithic budget to account for the fan-out.
    let budget = WORKFLOW_LOCKFILE_WRITE_MAX_MS * 2;
    eprintln!(
        "[perf] workflow_lockfile_split_write_100_stages_10_files: {elapsed_ms} ms (budget: < {budget} ms)"
    );
    assert!(
        elapsed_ms < budget,
        "split lockfile write (100 stages, 10 files) took {elapsed_ms} ms, exceeds budget of {budget} ms"
    );

    // Sanity: every declared file got a lockfile.
    for wf_path in &workflow_files {
        let lock_path = lockfile_split::lockfile_path_for(wf_path);
        let meta = std::fs::metadata(&lock_path).unwrap();
        assert!(meta.len() > 0);
    }
}

/// Single-mode write through the split layer must match the direct
/// `Lockfile::save` cost — the layer is a thin wrapper and should
/// not introduce measurable overhead for the common case.
#[test]
#[ignore]
fn perf_workflow_lockfile_single_through_split_layer_100_stages() {
    use crab::workflow::lockfile_split::{self, LockfileMode};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let mut lockfile = Lockfile::new();
    for idx in 0..100u32 {
        let name = StageName::parse(&format!("stage_{idx:03}")).unwrap();
        lockfile.stages.insert(name, make_locked_stage(idx));
    }

    let start = Instant::now();
    lockfile_split::save_lockfiles(
        root,
        &[],
        &Default::default(),
        &lockfile,
        LockfileMode::Single,
    )
    .unwrap();
    let elapsed = start.elapsed();

    let elapsed_ms = elapsed.as_millis();
    eprintln!(
        "[perf] workflow_lockfile_single_through_split_layer_100_stages: {elapsed_ms} ms (budget: < {WORKFLOW_LOCKFILE_WRITE_MAX_MS} ms)"
    );
    assert!(
        elapsed_ms < WORKFLOW_LOCKFILE_WRITE_MAX_MS,
        "single mode through split layer took {elapsed_ms} ms, exceeds budget of {WORKFLOW_LOCKFILE_WRITE_MAX_MS} ms"
    );
}
