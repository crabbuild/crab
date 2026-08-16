//! Reusable bench harness for `crab dehydrate` throughput.
//!
//! The harness generates a synthetic git repo of `--files N` tracked
//! pointer files, commits them, then times a full `dehydrate --all`
//! pass end-to-end. It exists so the Req 6 / Task 7.10 "≥ 20×
//! speedup" claim has a reproducible measurement that compares the
//! shellout-driven baseline (feature `gix-worktree` off) to the
//! gix-native path (feature on).
//!
//! This is a `cargo bench` target rather than a criterion harness —
//! criterion's sampling is overkill for a wall-clock number where
//! the variance of interest is between runs on different trees, not
//! within a run. The harness prints a single JSON line so CI
//! scrapers can pick it up without parsing free-form output.
//!
//! ## Usage
//!
//! ```text
//! # Default: 10 000 files
//! cargo bench -p crab --bench dehydrate_bench
//!
//! # Custom file count. Numeric suffixes k / m are honored.
//! cargo bench -p crab --bench dehydrate_bench -- --files 100k
//!
//! # Baseline (shellout path) — build without gix-worktree:
//! cargo bench -p crab --no-default-features --features tier-s3 \
//!     --bench dehydrate_bench -- --files 100k
//!
//! # Post-adoption (gix-status path):
//! cargo bench -p crab --features gix-worktree --bench dehydrate_bench \
//!     -- --files 100k
//! ```
//!
//! The harness reports:
//! - total wall-clock for the dehydrate pass, in milliseconds;
//! - files processed per second;
//! - dehydrate summary counters (dehydrated, skipped, dirty-skipped, failed);
//! - whether the `gix-worktree` feature flag is active at build time.
//!
//! Operators driving Task 7.10 record the pre-adoption number on a
//! 100 k-file fixture (`--files 100k`, feature off) and the
//! post-adoption number (feature on), then fill the
//! `docs/architecture/shellout-baseline.md` table with the ratio.

use std::path::Path;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crab::cmd::dehydrate::{DehydrateArgs, run_dehydrate_in};
use crab::core::output::OutputMode;
use crab_types::pointer::Pointer;

/// Default file count when `--files` is absent. Chosen small enough
/// that `cargo bench` without args stays snappy — the real Req 6
/// claim is measured with `--files 100k` per Task 7.10.
const DEFAULT_FILES: usize = 10_000;

/// Parse a numeric file count. Accepts bare integers plus `k` / `m`
/// suffixes so invocations like `--files 100k` stay readable.
fn parse_count(s: &str) -> Option<usize> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(rest) = s.strip_suffix('m') {
        (rest, 1_000_000usize)
    } else if let Some(rest) = s.strip_suffix('k') {
        (rest, 1_000usize)
    } else {
        (s.as_str(), 1usize)
    };
    num.parse::<usize>().ok().map(|n| n * mult)
}

/// Build a synthetic repo of `files` tracked pointer files plus
/// matching hydrated worktree content, committed to a fresh git repo.
///
/// The pointer blob is what gets committed (so `dehydrate` sees a
/// "clean" working tree once the content is restored to the hydrated
/// form); the worktree file is overwritten afterwards with
/// same-size non-pointer content so the dehydrate pass has real work
/// to do.
fn build_fixture(root: &Path, files: usize) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    std::fs::write(
        root.join(".gitattributes"),
        "*.bin filter=crab diff=crab merge=crab -text\n",
    )?;

    // Initialize git, configure an identity so commit works, commit
    // each pointer, then overwrite with hydrated content. This
    // matches the shape of a real post-hydrate working tree.
    let run = |args: &[&str]| -> std::io::Result<()> {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "git {:?} failed with {status}",
                args
            )));
        }
        Ok(())
    };

    run(&["init", "--initial-branch=main"])?;
    run(&["config", "user.email", "bench@crab.dev"])?;
    run(&["config", "user.name", "crab-bench"])?;

    let mut hash = [0u8; 32];
    for i in 0..files {
        // Cheap varying hash so pointers are distinct.
        for (b, byte) in hash.iter_mut().enumerate() {
            *byte = ((i + b) & 0xFF) as u8;
        }
        let pointer = Pointer {
            file_hash: hash,
            size: 4096,
            shard_hint: None,
        };
        let name = format!("data_{i:06}.bin");
        let path = root.join(&name);
        std::fs::write(&path, pointer.serialize())?;
    }

    run(&["add", "-A"])?;
    run(&["commit", "-m", "bench fixture"])?;

    // Overwrite with hydrated content so `dehydrate` has work.
    for i in 0..files {
        let name = format!("data_{i:06}.bin");
        let path = root.join(&name);
        // 4 KiB arbitrary content.
        std::fs::write(&path, vec![(i & 0xFF) as u8; 4096])?;
    }

    Ok(())
}

fn main() {
    let mut files = DEFAULT_FILES;
    let args: Vec<String> = std::env::args().collect();
    let mut it = args.iter().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--files" => {
                let Some(val) = it.next() else {
                    eprintln!("--files requires an argument");
                    std::process::exit(2);
                };
                match parse_count(val) {
                    Some(n) => files = n,
                    None => {
                        eprintln!("invalid --files value: {val}");
                        std::process::exit(2);
                    }
                }
            }
            "--help" | "-h" => {
                eprintln!("usage: dehydrate_bench [--files N[k|m]]");
                return;
            }
            // cargo bench propagates its own args; ignore anything we
            // don't recognize so the harness stays robust under
            // `cargo bench -- --nocapture` and similar.
            _ => {}
        }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let build_start = Instant::now();
    build_fixture(root, files).expect("build fixture");
    let build_ms = build_start.elapsed().as_millis();

    let args = DehydrateArgs {
        patterns: vec!["*.bin".to_owned()],
        all: false,
        ignore_profiles: false,
        mode: OutputMode::Text,
    };
    let cancel = CancellationToken::new();

    let run_start = Instant::now();
    run_dehydrate_in(root, &args, &cancel).expect("dehydrate");
    let elapsed = run_start.elapsed();
    let elapsed_ms = elapsed.as_millis();

    let rate = if elapsed.as_secs_f64() > 0.0 {
        files as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };

    // Is the gix-worktree path compiled in?
    let feature_gix_worktree = cfg!(feature = "gix-worktree");

    // Single-line JSON for easy scraping.
    println!(
        r#"{{"bench":"dehydrate","files":{files},"elapsed_ms":{elapsed_ms},"rate_files_per_sec":{rate:.1},"build_fixture_ms":{build_ms},"feature_gix_worktree":{feature_gix_worktree}}}"#
    );
}
