//! Synthetic repo pipeline baselines.
//!
//! Usage:
//!   cargo bench -p crab --bench repo_pipeline_bench -- --json
//!
//! The scenarios are local and deterministic so CI can run them without
//! object-store credentials. They are baselines, not pass/fail budgets.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crab::core::output::OutputMode;
use serde::Serialize;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Serialize)]
struct BenchRecord {
    scenario: &'static str,
    elapsed_ms: u64,
    bytes: u64,
    file_count: u64,
    pack_count: u64,
    peak_rss_bytes: Option<u64>,
    phases: Vec<BenchPhase>,
}

#[derive(Debug, Serialize)]
struct BenchPhase {
    phase: &'static str,
    elapsed_ms: u64,
    bytes_in: u64,
    bytes_out: u64,
    item_count: u64,
    peak_rss_bytes: Option<u64>,
}

fn main() {
    let _json = std::env::args().any(|arg| arg == "--json");
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let temp = tempfile::tempdir().expect("tempdir");

    let source = temp.path().join("source");
    create_source_repo(&source, 1_000, 8 * 1024 * 1024);

    let clone_parent = temp.path().join("clones");
    std::fs::create_dir_all(&clone_parent).expect("clone parent");

    emit(rt.block_on(cold_clone(&clone_parent, &source)));
    emit(rt.block_on(warm_clone(&clone_parent, &source)));

    let pull_repo = clone_parent.join("pull-work");
    clone_with_git(&source, &pull_repo);
    emit(fetch_noop(&pull_repo));
    add_commit(&source, "small-update.txt", b"small update\n");
    emit(pull_small_update(&pull_repo));

    emit(push_noop(temp.path()));
    emit(push_many_small_files(temp.path()));
    emit(push_large_files(temp.path()));
}

async fn cold_clone(parent: &Path, source: &Path) -> BenchRecord {
    let target = PathBuf::from("cold");
    let elapsed = measure_async(async {
        let args = crab::cmd::clone::CloneArgs {
            url: source.display().to_string(),
            directory: Some(target),
            branch: None,
            depth: None,
            lazy: true,
            include: vec![],
            exclude: vec![],
            sync_chunk_index: false,
            mode: OutputMode::Json,
        };
        crab::cmd::clone::run_clone_in(parent, &args, &CancellationToken::new())
            .await
            .expect("cold clone");
    })
    .await;

    let repo = parent.join("cold");
    let bytes = dir_size(&repo);
    let files = count_files(&repo);
    let packs = count_pack_files(&repo.join(".git/objects/pack"));
    BenchRecord {
        scenario: "cold_clone",
        elapsed_ms: elapsed,
        bytes,
        file_count: files,
        pack_count: packs,
        peak_rss_bytes: rss(),
        phases: vec![phase("pack_fetch_checkout", elapsed, 0, bytes, files)],
    }
}

async fn warm_clone(parent: &Path, source: &Path) -> BenchRecord {
    let target = PathBuf::from("warm");
    let elapsed = measure_async(async {
        let args = crab::cmd::clone::CloneArgs {
            url: source.display().to_string(),
            directory: Some(target),
            branch: None,
            depth: None,
            lazy: true,
            include: vec![],
            exclude: vec![],
            sync_chunk_index: false,
            mode: OutputMode::Json,
        };
        crab::cmd::clone::run_clone_in(parent, &args, &CancellationToken::new())
            .await
            .expect("warm clone");
    })
    .await;

    let repo = parent.join("warm");
    let bytes = dir_size(&repo);
    let files = count_files(&repo);
    let packs = count_pack_files(&repo.join(".git/objects/pack"));
    BenchRecord {
        scenario: "warm_clone",
        elapsed_ms: elapsed,
        bytes,
        file_count: files,
        pack_count: packs,
        peak_rss_bytes: rss(),
        phases: vec![phase("pack_fetch_checkout", elapsed, 0, bytes, files)],
    }
}

fn fetch_noop(repo: &Path) -> BenchRecord {
    let elapsed = measure(|| {
        git(repo, &["fetch", "origin"]);
    });
    let bytes = dir_size(repo);
    let files = count_files(repo);
    let packs = count_pack_files(&repo.join(".git/objects/pack"));
    BenchRecord {
        scenario: "fetch_noop",
        elapsed_ms: elapsed,
        bytes,
        file_count: files,
        pack_count: packs,
        peak_rss_bytes: rss(),
        phases: vec![phase("pack_fetch", elapsed, 0, 0, packs)],
    }
}

fn pull_small_update(repo: &Path) -> BenchRecord {
    let fetch_elapsed = measure(|| {
        git(repo, &["fetch", "origin", "main"]);
    });
    let checkout_elapsed = measure(|| {
        git(repo, &["merge", "--ff-only", "FETCH_HEAD"]);
    });
    let elapsed = fetch_elapsed + checkout_elapsed;
    let bytes = dir_size(repo);
    let files = count_files(repo);
    let packs = count_pack_files(&repo.join(".git/objects/pack"));
    BenchRecord {
        scenario: "pull_small_update",
        elapsed_ms: elapsed,
        bytes,
        file_count: files,
        pack_count: packs,
        peak_rss_bytes: rss(),
        phases: vec![
            phase("pack_fetch", fetch_elapsed, 0, 0, packs),
            phase("checkout", checkout_elapsed, 0, bytes, files),
        ],
    }
}

fn push_noop(root: &Path) -> BenchRecord {
    let (_remote, work) = push_fixture(root, "push-noop");
    let elapsed = measure(|| {
        git(&work, &["push", "origin", "main"]);
    });
    let bytes = dir_size(&work);
    let files = count_files(&work);
    let packs = count_pack_files(&work.join(".git/objects/pack"));
    BenchRecord {
        scenario: "push_noop",
        elapsed_ms: elapsed,
        bytes,
        file_count: files,
        pack_count: packs,
        peak_rss_bytes: rss(),
        phases: vec![phase("pack_upload", elapsed, bytes, 0, packs)],
    }
}

fn push_many_small_files(root: &Path) -> BenchRecord {
    let (_remote, work) = push_fixture(root, "push-many-small");
    for i in 0..2_000 {
        let path = work.join(format!("tiny/file-{i:04}.txt"));
        std::fs::create_dir_all(path.parent().unwrap()).expect("tiny dir");
        std::fs::write(path, format!("payload {i}\n")).expect("tiny file");
    }
    git(&work, &["add", "tiny"]);
    git(&work, &["commit", "-m", "many small"]);
    let elapsed = measure(|| {
        git(&work, &["push", "origin", "main"]);
    });
    let bytes = dir_size(&work);
    let files = count_files(&work);
    let packs = count_pack_files(&work.join(".git/objects/pack"));
    BenchRecord {
        scenario: "push_many_small_files",
        elapsed_ms: elapsed,
        bytes,
        file_count: files,
        pack_count: packs,
        peak_rss_bytes: rss(),
        phases: vec![phase("pack_upload", elapsed, bytes, 0, files)],
    }
}

fn push_large_files(root: &Path) -> BenchRecord {
    let (_remote, work) = push_fixture(root, "push-large");
    std::fs::write(work.join("large-a.bin"), vec![7u8; 16 * 1024 * 1024]).expect("large a");
    std::fs::write(work.join("large-b.bin"), vec![9u8; 16 * 1024 * 1024]).expect("large b");
    git(&work, &["add", "large-a.bin", "large-b.bin"]);
    git(&work, &["commit", "-m", "large files"]);
    let elapsed = measure(|| {
        git(&work, &["push", "origin", "main"]);
    });
    let bytes = dir_size(&work);
    let files = count_files(&work);
    let packs = count_pack_files(&work.join(".git/objects/pack"));
    BenchRecord {
        scenario: "push_large_files",
        elapsed_ms: elapsed,
        bytes,
        file_count: files,
        pack_count: packs,
        peak_rss_bytes: rss(),
        phases: vec![phase("pack_upload", elapsed, bytes, 0, files)],
    }
}

fn create_source_repo(path: &Path, small_files: usize, large_bytes: usize) {
    std::fs::create_dir_all(path).expect("source dir");
    git(path, &["init", "-b", "main"]);
    git(path, &["config", "user.email", "bench@crab.dev"]);
    git(path, &["config", "user.name", "crab bench"]);
    for i in 0..small_files {
        let p = path.join(format!("src/file-{i:04}.txt"));
        std::fs::create_dir_all(p.parent().unwrap()).expect("src dir");
        std::fs::write(p, format!("line {i}\n")).expect("small file");
    }
    std::fs::write(path.join("large.bin"), vec![3u8; large_bytes]).expect("large file");
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "initial"]);
}

fn add_commit(repo: &Path, file: &str, contents: &[u8]) {
    std::fs::write(repo.join(file), contents).expect("write update");
    git(repo, &["add", file]);
    git(repo, &["commit", "-m", "small update"]);
}

fn clone_with_git(source: &Path, target: &Path) {
    let output = Command::new("git")
        .arg("clone")
        .arg(source)
        .arg(target)
        .output()
        .expect("git clone spawn");
    assert!(
        output.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn push_fixture(root: &Path, name: &str) -> (TempDir, PathBuf) {
    let remote = tempfile::tempdir_in(root).expect("bare remote");
    git(remote.path(), &["init", "--bare", "-b", "main"]);

    let work = root.join(name);
    std::fs::create_dir_all(&work).expect("work dir");
    git(&work, &["init", "-b", "main"]);
    git(&work, &["config", "user.email", "bench@crab.dev"]);
    git(&work, &["config", "user.name", "crab bench"]);
    std::fs::write(work.join("README.md"), b"bench\n").expect("readme");
    git(&work, &["add", "README.md"]);
    git(&work, &["commit", "-m", "initial"]);
    git(
        &work,
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    git(&work, &["push", "-u", "origin", "main"]);
    (remote, work)
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("git spawn");
    assert!(
        output.status.success(),
        "git {:?} failed in {}:\nstdout={}\nstderr={}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn measure(f: impl FnOnce()) -> u64 {
    let start = Instant::now();
    f();
    start.elapsed().as_millis() as u64
}

async fn measure_async(f: impl std::future::Future<Output = ()>) -> u64 {
    let start = Instant::now();
    f.await;
    start.elapsed().as_millis() as u64
}

fn emit(record: BenchRecord) {
    println!("{}", serde_json::to_string(&record).expect("json"));
}

fn rss() -> Option<u64> {
    memory_stats::memory_stats().map(|m| m.physical_mem as u64)
}

fn phase(
    phase: &'static str,
    elapsed_ms: u64,
    bytes_in: u64,
    bytes_out: u64,
    item_count: u64,
) -> BenchPhase {
    BenchPhase {
        phase,
        elapsed_ms,
        bytes_in,
        bytes_out,
        item_count,
        peak_rss_bytes: rss(),
    }
}

fn count_files(path: &Path) -> u64 {
    walk(path).into_iter().filter(|p| p.is_file()).count() as u64
}

fn count_pack_files(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pack"))
        .count() as u64
}

fn dir_size(path: &Path) -> u64 {
    walk(path)
        .into_iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

fn walk(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![path.to_owned()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}
