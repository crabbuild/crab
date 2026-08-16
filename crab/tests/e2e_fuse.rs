//! End-to-end FUSE mount tests for the crab VFS.
//!
//! Mirrors artifact-fs's `e2e_test.go` structure: creates a local bare
//! git repo, starts the daemon, mounts via FUSE, and exercises filesystem
//! and git operations against the live mount.
//!
//! Gated behind:
//! - `#[cfg(feature = "fuse")]` — compile-time gate
//! - `CRAB_RUN_FUSE_E2E=1` — runtime gate (CI without FUSE skips)

#![cfg(feature = "fuse")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Runtime gate
// ---------------------------------------------------------------------------

/// Returns `true` when the test should be skipped (env var not set).
fn skip_unless_fuse_e2e() -> bool {
    std::env::var("CRAB_RUN_FUSE_E2E").map_or(true, |v| v != "1")
}

// ---------------------------------------------------------------------------
// Local bare repo seeding
// ---------------------------------------------------------------------------

/// Seed a local bare git repo with 3 commits containing tracked files
/// and a subdirectory tree. Returns the `file://` URL for blobless clone.
fn create_local_test_repo(tmp: &Path) -> String {
    let bare_dir = tmp.join("test-repo.git");
    let work_dir = tmp.join("work");

    // Init bare repo.
    git_run(None, &["init", "--bare", &bare_dir.to_string_lossy()]);

    // Clone into a working tree.
    git_run(
        None,
        &[
            "clone",
            &bare_dir.to_string_lossy(),
            &work_dir.to_string_lossy(),
        ],
    );
    git_run(Some(&work_dir), &["config", "user.name", "E2E Setup"]);
    git_run(Some(&work_dir), &["config", "user.email", "e2e@test"]);
    git_run(Some(&work_dir), &["checkout", "-b", "main"]);

    // Commit 1: readme, license, security files.
    write_test_file(
        &work_dir,
        "README.md",
        "# Test Repo\n\nE2E test repository.\n",
    );
    write_test_file(
        &work_dir,
        "LICENSE-MIT",
        "MIT License\n\nCopyright 2024 Test\n\nPermission is hereby granted.\n",
    );
    write_test_file(
        &work_dir,
        "SECURITY.md",
        "# Security\n\nReport security issues responsibly.\n",
    );
    git_run(Some(&work_dir), &["add", "-A"]);
    git_run(Some(&work_dir), &["commit", "-m", "add readme and license"]);

    // Commit 2: root package manifest.
    write_test_file(
        &work_dir,
        "package.json",
        "{\"name\":\"e2e-test-repo\",\"version\":\"1.0.0\"}\n",
    );
    git_run(Some(&work_dir), &["add", "-A"]);
    git_run(Some(&work_dir), &["commit", "-m", "add package manifest"]);

    // Commit 3: packages directory with subdirectories.
    for pkg in &["wrangler", "miniflare", "vitest-pool", "workers-shared"] {
        let dir = work_dir.join("packages").join(pkg);
        std::fs::create_dir_all(&dir).unwrap();
        write_test_file(
            &dir,
            "package.json",
            &format!("{{\"name\":\"{pkg}\",\"version\":\"0.0.1\"}}\n"),
        );
    }
    git_run(Some(&work_dir), &["add", "-A"]);
    git_run(Some(&work_dir), &["commit", "-m", "add packages directory"]);

    git_run(Some(&work_dir), &["push", "origin", "main"]);

    format!("file://{}", bare_dir.display())
}

fn write_test_file(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
}

// ---------------------------------------------------------------------------
// Git command helpers
// ---------------------------------------------------------------------------

fn git_run(dir: Option<&Path>, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
        // macOS resolves /tmp → /private/tmp; add both as safe directories.
        cmd.args(["-c", &format!("safe.directory={}", d.display())]);
        let display = d.display().to_string();
        if display.starts_with("/tmp/") {
            cmd.args(["-c", &format!("safe.directory=/private{display}")]);
        }
    }
    cmd.args(args);
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("git {}: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed:\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Run a git command, returning (stdout, stderr, success).
fn git_cmd_result(dir: &Path, args: &[&str]) -> (String, String, bool) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir);
    cmd.args(["-c", &format!("safe.directory={}", dir.display())]);
    let display = dir.display().to_string();
    if display.starts_with("/tmp/") {
        cmd.args(["-c", &format!("safe.directory=/private{display}")]);
    }
    cmd.args(args);
    let output = cmd.output().unwrap();
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

// ---------------------------------------------------------------------------
// Mount detection
// ---------------------------------------------------------------------------

fn is_mounted(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("mount").output().ok();
        output.map_or(false, |o| {
            let s = String::from_utf8_lossy(&o.stdout);
            let p = path.display().to_string();
            s.contains(&p) || s.contains(&format!("/private{p}"))
        })
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/mounts")
            .map_or(false, |s| s.contains(&path.display().to_string()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
        false
    }
}

fn wait_for_mount(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_mounted(path) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

fn wait_for_unmount(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_mounted(path) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

fn ls_dir(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect()
}

fn read_file_str(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Read a file with retries (FUSE may need a moment to serve content).
fn read_file_eventually(path: &Path, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last_err = None;
    while Instant::now() < deadline {
        match std::fs::read_to_string(path) {
            Ok(s) => return s,
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("failed to read {}: {}", path.display(), last_err.unwrap());
}

// ---------------------------------------------------------------------------
// Daemon harness
// ---------------------------------------------------------------------------

/// Full E2E harness: creates temp dirs, seeds a repo, starts the daemon,
/// waits for mount, and returns paths for test assertions.
struct E2eHarness {
    /// Temp directory root (cleaned up on drop).
    _tmp: tempfile::TempDir,
    /// Path where the FUSE filesystem is mounted.
    mount_path: PathBuf,
    /// Daemon root directory.
    daemon_root: PathBuf,
    /// Tokio runtime for the daemon (held to keep it alive).
    #[allow(dead_code, reason = "held to keep the runtime alive for the daemon")]
    rt: tokio::runtime::Runtime,
    /// Cancellation token to stop the daemon.
    cancel: tokio_util::sync::CancellationToken,
}

impl E2eHarness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let daemon_root = tmp.path().join("daemon");
        let mount_dir = tmp.path().join("mounts");
        std::fs::create_dir_all(&daemon_root).unwrap();
        std::fs::create_dir_all(&mount_dir).unwrap();

        let remote_url = create_local_test_repo(tmp.path());

        let cancel = tokio_util::sync::CancellationToken::new();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let mount_path = mount_dir.join("e2e-test");
        std::fs::create_dir_all(&mount_path).unwrap();

        // Create and start the daemon.
        let daemon = rt.block_on(async {
            let svc =
                crab::vfs::daemon::DaemonService::new(daemon_root.clone(), cancel.clone()).unwrap();

            let config = crab::vfs::daemon::RepoConfig {
                name: "e2e-test".to_owned(),
                remote: remote_url.clone(),
                remote_redacted: remote_url,
                branch: "main".to_owned(),
                mount_root: mount_dir.to_string_lossy().to_string(),
                refresh_interval_secs: 300,
                enabled: true,
                read_only: false,
                backend: crab::vfs::daemon::DaemonMountBackend::Fuse,
            };
            svc.registry().add_repo(&config).unwrap();
            svc.start().await.unwrap();
            svc
        });

        // Wait for the FUSE mount to appear.
        assert!(
            wait_for_mount(&mount_path, Duration::from_secs(60)),
            "FUSE mount did not appear at {} within 60s",
            mount_path.display()
        );

        // Keep daemon alive by leaking it into a static — it will be
        // cleaned up when we cancel the token.
        std::mem::forget(daemon);

        Self {
            _tmp: tmp,
            mount_path,
            daemon_root,
            rt,
            cancel,
        }
    }

    fn mp(&self) -> &Path {
        &self.mount_path
    }
}

impl Drop for E2eHarness {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Give the daemon time to unmount.
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ===========================================================================
// Test functions
// ===========================================================================

// ---------------------------------------------------------------------------
// 79.1: Test harness — verify the harness itself works
// ---------------------------------------------------------------------------

#[test]
fn e2e_harness_creates_local_repo() {
    if skip_unless_fuse_e2e() {
        return;
    }
    let _harness = E2eHarness::new();
    // If we get here, the harness created the repo, started the daemon,
    // and the FUSE mount appeared. The Drop impl will clean up.
}

// ---------------------------------------------------------------------------
// 79.2: Filesystem read operations
// ---------------------------------------------------------------------------

#[test]
fn e2e_fs_read_operations() {
    if skip_unless_fuse_e2e() {
        return;
    }
    let h = E2eHarness::new();
    let mp = h.mp();

    // ls root — should have at least .git, README.md, LICENSE-MIT, etc.
    let entries = ls_dir(mp);
    assert!(
        entries.len() >= 4,
        "expected ≥4 root entries, got {}: {entries:?}",
        entries.len()
    );
    assert!(
        entries.contains(&".git".to_owned()),
        "root must contain .git: {entries:?}"
    );

    // cat tracked file.
    let readme = read_file_eventually(&mp.join("README.md"), Duration::from_secs(5));
    assert!(!readme.is_empty(), "README.md must not be empty");
    assert!(
        readme.contains("Test Repo"),
        "README.md must contain 'Test Repo'"
    );

    // stat file size.
    let meta = std::fs::metadata(mp.join("package.json")).unwrap();
    assert!(meta.len() > 0, "package.json must have non-zero size");

    // ls subdirectory.
    let pkg_entries = ls_dir(&mp.join("packages"));
    assert!(
        pkg_entries.len() >= 4,
        "expected ≥4 packages/ entries, got {}: {pkg_entries:?}",
        pkg_entries.len()
    );

    // Read nested file.
    let nested = read_file_str(&mp.join("packages/wrangler/package.json"));
    assert!(
        nested.contains("wrangler"),
        "nested package.json must contain 'wrangler'"
    );

    // Read .git gitfile.
    let gitfile = read_file_str(&mp.join(".git"));
    assert!(
        gitfile.starts_with("gitdir:"),
        ".git must start with 'gitdir:'"
    );
}

// ---------------------------------------------------------------------------
// 79.3: Filesystem write operations
// ---------------------------------------------------------------------------

#[test]
fn e2e_fs_write_operations() {
    if skip_unless_fuse_e2e() {
        return;
    }
    let h = E2eHarness::new();
    let mp = h.mp();

    // Create file.
    let test_file = mp.join("e2e-test-file.txt");
    std::fs::write(&test_file, "hello e2e\n").unwrap();
    assert_eq!(read_file_str(&test_file), "hello e2e\n");

    // mkdir.
    let test_dir = mp.join("e2e-test-dir");
    std::fs::create_dir(&test_dir).unwrap();
    assert!(std::fs::metadata(&test_dir).unwrap().is_dir());

    // Write nested file.
    let nested = test_dir.join("nested.txt");
    std::fs::write(&nested, "nested\n").unwrap();
    assert_eq!(read_file_str(&nested), "nested\n");

    // Rename.
    let renamed = mp.join("e2e-renamed.txt");
    std::fs::rename(&test_file, &renamed).unwrap();
    assert_eq!(read_file_str(&renamed), "hello e2e\n");
    assert!(!test_file.exists());

    // Unlink.
    std::fs::remove_file(&renamed).unwrap();
    assert!(!renamed.exists());

    // Rmdir (remove nested file first).
    std::fs::remove_file(&nested).unwrap();
    std::fs::remove_dir(&test_dir).unwrap();
    assert!(!test_dir.exists());

    // Modify tracked file.
    let readme = mp.join("README.md");
    let orig = read_file_eventually(&readme, Duration::from_secs(5));
    let modified = format!("{orig}# e2e test marker\n");
    std::fs::write(&readme, &modified).unwrap();
    let got = read_file_str(&readme);
    assert!(
        got.ends_with("# e2e test marker\n"),
        "modified README must end with marker"
    );

    // Rename tracked file.
    let security_src = mp.join("SECURITY.md");
    let security_dst = mp.join("SECURITY.bak");
    std::fs::rename(&security_src, &security_dst).unwrap();
    let data = read_file_str(&security_dst);
    assert!(!data.is_empty(), "renamed tracked file must not be empty");
    assert!(!security_src.exists());

    // Truncate tracked file.
    let license = mp.join("LICENSE-MIT");
    // Read first to trigger hydration.
    let _ = read_file_eventually(&license, Duration::from_secs(5));
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&license)
        .unwrap();
    f.set_len(0).unwrap();
    drop(f);
    let meta = std::fs::metadata(&license).unwrap();
    assert_eq!(meta.len(), 0, "LICENSE-MIT must be 0 bytes after truncate");
}

// ---------------------------------------------------------------------------
// 79.4: Git operations
// ---------------------------------------------------------------------------

#[test]
fn e2e_git_read_operations() {
    if skip_unless_fuse_e2e() {
        return;
    }
    let h = E2eHarness::new();
    let mp = h.mp();

    // git log — 3 commits.
    let log = git_run(Some(mp), &["log", "--oneline", "-3"]);
    let lines: Vec<&str> = log.trim().lines().collect();
    assert!(
        lines.len() >= 3,
        "expected ≥3 log lines, got {}: {log}",
        lines.len()
    );

    // git branch.
    let branch = git_run(Some(mp), &["branch"]);
    assert!(branch.contains("main"), "branch output must contain 'main'");

    // git rev-parse HEAD — 40-char SHA.
    let head = git_run(Some(mp), &["rev-parse", "HEAD"]);
    assert_eq!(
        head.trim().len(),
        40,
        "HEAD must be 40-char SHA, got: {}",
        head.trim()
    );

    // git show.
    let show = git_run(Some(mp), &["show", "HEAD", "--stat", "--format=%H"]);
    assert!(!show.is_empty(), "git show must produce output");

    // git remote -v.
    let remote = git_run(Some(mp), &["remote", "-v"]);
    assert!(
        remote.contains("origin"),
        "remote output must contain 'origin'"
    );

    // git stash list (should not error, output may be empty).
    let (_, _, ok) = git_cmd_result(mp, &["stash", "list"]);
    assert!(ok, "git stash list must succeed");
}

#[test]
fn e2e_git_write_operations() {
    if skip_unless_fuse_e2e() {
        return;
    }
    let h = E2eHarness::new();
    let mp = h.mp();

    // Modify a tracked file first.
    let readme = mp.join("README.md");
    let orig = read_file_eventually(&readme, Duration::from_secs(5));
    std::fs::write(&readme, format!("{orig}# e2e marker\n")).unwrap();

    // git diff.
    let diff = git_run(Some(mp), &["diff", "README.md"]);
    assert!(
        diff.contains("e2e marker"),
        "diff must contain 'e2e marker'"
    );

    // git add.
    git_run(Some(mp), &["add", "README.md"]);
    let status = git_run(Some(mp), &["status", "--short", "README.md"]);
    assert!(
        status.trim_start().starts_with('M'),
        "README.md must be staged: {status}"
    );

    // git reset.
    git_run(Some(mp), &["reset", "HEAD", "README.md"]);

    // git status — should show modifications.
    let status = git_run(Some(mp), &["status", "--short"]);
    assert!(
        !status.trim().is_empty(),
        "status must be non-empty after modifications"
    );
}

// ---------------------------------------------------------------------------
// 79.5: Post-commit reconciliation
// ---------------------------------------------------------------------------

#[test]
fn e2e_post_commit_reconciliation() {
    if skip_unless_fuse_e2e() {
        return;
    }
    let h = E2eHarness::new();
    let mp = h.mp();

    let pre_head = git_run(Some(mp), &["rev-parse", "HEAD"]);
    let pre_head = pre_head.trim();

    // Create and stage a new file.
    let commit_file = mp.join("e2e-commit.txt");
    std::fs::write(&commit_file, "committed content\n").unwrap();
    git_run(Some(mp), &["add", "e2e-commit.txt"]);

    // Commit.
    git_run(
        Some(mp),
        &[
            "-c",
            "user.name=E2E Test",
            "-c",
            "user.email=e2e@test",
            "commit",
            "-m",
            "e2e commit test",
        ],
    );

    // Poll until reconciliation completes: the HEAD watcher detects the
    // change, rebuilds the snapshot, reconciles the overlay, and refreshes
    // the git index. When `git status` reports the committed file as clean,
    // all of that has finished.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut reconciled = false;
    while Instant::now() < deadline {
        let (out, _, _) = git_cmd_result(mp, &["status", "--short", "e2e-commit.txt"]);
        if out.trim().is_empty() {
            reconciled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        reconciled,
        "overlay reconciliation did not complete within 10s"
    );

    // HEAD must have advanced.
    let post_head = git_run(Some(mp), &["rev-parse", "HEAD"]);
    assert_ne!(post_head.trim(), pre_head, "HEAD must change after commit");

    // Log should contain our commit message.
    let log = git_run(Some(mp), &["log", "--oneline", "-1"]);
    assert!(
        log.contains("e2e commit test"),
        "log must contain commit message"
    );

    // File content should still be readable from the base snapshot.
    let content = read_file_str(&commit_file);
    assert_eq!(content, "committed content\n");
}

// ---------------------------------------------------------------------------
// 79.6: Clean daemon shutdown
// ---------------------------------------------------------------------------

#[test]
fn e2e_clean_daemon_shutdown() {
    if skip_unless_fuse_e2e() {
        return;
    }
    let h = E2eHarness::new();
    let mp = h.mp().to_path_buf();
    let daemon_root = h.daemon_root.clone();

    // Verify mount is active before shutdown.
    assert!(is_mounted(&mp), "mount must be active before shutdown");

    // Cancel the daemon.
    h.cancel.cancel();

    // Wait for unmount.
    assert!(
        wait_for_unmount(&mp, Duration::from_secs(10)),
        "FUSE must unmount within 10s after cancel"
    );

    // PID file should be removed.
    let pid_file = daemon_root.join("repos/e2e-test/.crab/mount.pid");
    // Give a moment for cleanup.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !pid_file.exists(),
        "PID file must be removed after shutdown"
    );
}
