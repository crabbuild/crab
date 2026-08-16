//! Integration tests for `crab workflow lockfile resolve`.
//!
//! Synthesizes a git-merge-conflict `crab.lock` (two canonical
//! lockfile blocks glued together with git's `<<<<<<< / ======= /
//! >>>>>>>` markers), runs the three resolution modes, and locks in
//! the R5 byte-equality invariant: `--recompute` produces the same
//! resolved bytes regardless of which side of the conflict invoked
//! the command.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Minimal canonical lockfile body. The shape matches what
/// `Lockfile::serialize_canonical()` emits in the library; we
/// hand-roll it here to keep the integration test free of library
/// re-exports.
///
/// `stage_hash` carries the per-side digest so `ours` and `theirs`
/// diverge at the stage level, which is what makes recompute drop
/// the stage.
fn lockfile_body(stage_hash_hex: &str) -> String {
    // Note: `0xab` repeated 64 times for deps/outs — any fixed value
    // works; the byte-equality test only needs the two sides to
    // differ at `stage_hash`.
    let fixed_hex = "ab".repeat(32);
    format!(
        concat!(
            "crab_hash_algo: \"crab.stage.v1\"\n",
            "schema_version: 1\n",
            "stages:\n",
            "  train:\n",
            "    attempts: 1\n",
            "    cmd:\n",
            "      shell: \"python train.py\"\n",
            "    deps:\n",
            "      - hash: \"b3:{dep_hex}\"\n",
            "        path: \"src/train.py\"\n",
            "        size: 1234\n",
            "    duration_ms: 10\n",
            "    env: {{}}\n",
            "    executed_at: \"2026-04-27T14:23:11.083Z\"\n",
            "    host_fingerprint: \"linux-x86_64-crab-0.8.0\"\n",
            "    metrics: []\n",
            "    outs:\n",
            "      - hash: \"b3:{out_hex}\"\n",
            "        kind: \"file\"\n",
            "        mode: \"0o644\"\n",
            "        path: \"models/model.pkl\"\n",
            "        size: 4096\n",
            "    params: {{}}\n",
            "    stage_hash: \"b3:{stage_hex}\"\n",
        ),
        dep_hex = fixed_hex,
        out_hex = fixed_hex,
        stage_hex = stage_hash_hex,
    )
}

/// Build a git-merge-conflict file containing two full lockfile
/// blocks under `<<<<<<< / ======= / >>>>>>>` markers.
fn make_conflicted_file(ours_stage_hex: &str, theirs_stage_hex: &str) -> String {
    let mut out = String::new();
    out.push_str("<<<<<<< HEAD\n");
    out.push_str(&lockfile_body(ours_stage_hex));
    out.push_str("=======\n");
    out.push_str(&lockfile_body(theirs_stage_hex));
    out.push_str(">>>>>>> theirs\n");
    out
}

/// Spawn `crab workflow lockfile resolve` with the given strategy
/// flag in `repo`. Returns the command's exit status and captured
/// stdout bytes.
fn run_resolve(repo: &Path, flag: &str) -> (std::process::ExitStatus, Vec<u8>) {
    let mut cmd = Command::new(bin());
    cmd.current_dir(repo)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["workflow", "lockfile", "resolve", "--json", flag]);
    let output = cmd
        .output()
        .expect("crab workflow lockfile resolve should spawn");
    (output.status, output.stdout)
}

#[test]
fn ours_strategy_picks_ours_side() {
    let ours_stage = "11".repeat(32);
    let theirs_stage = "22".repeat(32);

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("crab.lock");
    fs::write(&lockfile, make_conflicted_file(&ours_stage, &theirs_stage)).unwrap();

    let (status, stdout) = run_resolve(tmp.path(), "--ours");
    assert!(status.success(), "resolve --ours should succeed");

    let envelope: serde_json::Value = serde_json::from_slice(&stdout).unwrap_or_else(|e| {
        panic!(
            "parse --json: {e}; stdout={}",
            String::from_utf8_lossy(&stdout)
        )
    });
    assert_eq!(envelope["data"]["strategy"], "ours");
    assert_eq!(
        envelope["data"]["stages_dropped"].as_array().unwrap().len(),
        0
    );

    let resolved = fs::read_to_string(&lockfile).unwrap();
    assert!(
        resolved.contains(&format!("b3:{ours_stage}")),
        "resolved file missing ours stage hash:\n{resolved}"
    );
    assert!(
        !resolved.contains(&format!("b3:{theirs_stage}")),
        "resolved file should not carry theirs hash under --ours"
    );
}

#[test]
fn theirs_strategy_picks_theirs_side() {
    let ours_stage = "11".repeat(32);
    let theirs_stage = "22".repeat(32);

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("crab.lock");
    fs::write(&lockfile, make_conflicted_file(&ours_stage, &theirs_stage)).unwrap();

    let (status, stdout) = run_resolve(tmp.path(), "--theirs");
    assert!(status.success(), "resolve --theirs should succeed");

    let envelope: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(envelope["data"]["strategy"], "theirs");

    let resolved = fs::read_to_string(&lockfile).unwrap();
    assert!(
        resolved.contains(&format!("b3:{theirs_stage}")),
        "resolved file missing theirs stage hash:\n{resolved}"
    );
}

#[test]
fn recompute_default_strategy_runs_when_no_flag_given() {
    // R5 default: omitting all three flags selects --recompute.
    let ours_stage = "aa".repeat(32);
    let theirs_stage = "bb".repeat(32);

    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("crab.lock");
    fs::write(&lockfile, make_conflicted_file(&ours_stage, &theirs_stage)).unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["workflow", "lockfile", "resolve", "--json"])
        .output()
        .expect("crab workflow lockfile resolve should spawn");
    assert!(output.status.success(), "default resolve should succeed");

    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["data"]["strategy"], "recompute");
    // The single conflicting stage has divergent stage_hash, so
    // recompute drops it.
    assert_eq!(
        envelope["data"]["stages_dropped"].as_array().unwrap().len(),
        1
    );
}

#[test]
fn recompute_is_byte_identical_regardless_of_side_order() {
    // Build the conflict two ways — (A ours / B theirs) and
    // (B ours / A theirs) — run recompute on both, assert the
    // resolved files match byte-for-byte.
    let stage_a = "aa".repeat(32);
    let stage_b = "bb".repeat(32);

    let tmp_ab = TempDir::new().unwrap();
    let lockfile_ab = tmp_ab.path().join("crab.lock");
    fs::write(&lockfile_ab, make_conflicted_file(&stage_a, &stage_b)).unwrap();

    let tmp_ba = TempDir::new().unwrap();
    let lockfile_ba = tmp_ba.path().join("crab.lock");
    fs::write(&lockfile_ba, make_conflicted_file(&stage_b, &stage_a)).unwrap();

    let (status_ab, _) = run_resolve(tmp_ab.path(), "--recompute");
    assert!(status_ab.success());
    let (status_ba, _) = run_resolve(tmp_ba.path(), "--recompute");
    assert!(status_ba.success());

    let bytes_ab = fs::read(&lockfile_ab).unwrap();
    let bytes_ba = fs::read(&lockfile_ba).unwrap();
    assert_eq!(
        bytes_ab, bytes_ba,
        "recompute must produce byte-identical output regardless of side order"
    );
}

#[test]
fn resolve_on_clean_file_exits_nonzero() {
    // A non-conflicted lockfile should fail loudly rather than
    // silently rewrite itself.
    let tmp = TempDir::new().unwrap();
    let lockfile = tmp.path().join("crab.lock");
    fs::write(&lockfile, lockfile_body(&"cc".repeat(32))).unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["workflow", "lockfile", "resolve", "--recompute"])
        .output()
        .expect("crab workflow lockfile resolve should spawn");
    assert!(!output.status.success(), "clean file must fail");
}
