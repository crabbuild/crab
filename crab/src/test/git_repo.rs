//! Shared test infrastructure for tests that need a real git repository.
//!
//! Provides a lazily-initialised temporary git repo with a single commit
//! on `refs/heads/main`, plus `refs/heads/dev` and `refs/tags/v1.0`.
//! A process-wide mutex serialises access to the `GIT_DIR` env var so
//! tests from different modules don't interfere.

use std::sync::{LazyLock, Mutex, MutexGuard};

/// Temporary git repo created once per test binary.
pub static TEST_GIT_REPO: LazyLock<TestGitRepo> = LazyLock::new(TestGitRepo::create);

/// Process-wide mutex for `GIT_DIR` env var manipulation.
pub static GIT_DIR_MUTEX: Mutex<()> = Mutex::new(());

/// Process-wide mutex for `CRAB_CACHE_DIR` env var manipulation.
///
/// Tests that override the cache root via `CRAB_CACHE_DIR` must hold
/// this mutex while the env var is set. Env vars are process-global;
/// without serialisation a parallel test might read a sibling test's
/// temp directory and fail spuriously.
pub static CACHE_DIR_MUTEX: Mutex<()> = Mutex::new(());

/// RAII guard that clears Git's process-global repository overrides.
///
/// Tests that shell out to `git` with `current_dir` still inherit Git env;
/// holding this guard prevents sibling overrides from redirecting local config writes.
pub struct CleanGitEnvGuard {
    _lock: MutexGuard<'static, ()>,
    prev_git_dir: Option<String>,
    prev_git_work_tree: Option<String>,
    prev_git_common_dir: Option<String>,
}

impl CleanGitEnvGuard {
    pub fn new() -> Self {
        let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev_git_dir = std::env::var("GIT_DIR").ok();
        let prev_git_work_tree = std::env::var("GIT_WORK_TREE").ok();
        let prev_git_common_dir = std::env::var("GIT_COMMON_DIR").ok();
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        unsafe {
            std::env::remove_var("GIT_DIR");
            std::env::remove_var("GIT_WORK_TREE");
            std::env::remove_var("GIT_COMMON_DIR");
        }
        Self {
            _lock: lock,
            prev_git_dir,
            prev_git_work_tree,
            prev_git_common_dir,
        }
    }
}

impl Default for CleanGitEnvGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CleanGitEnvGuard {
    fn drop(&mut self) {
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        unsafe {
            match &self.prev_git_dir {
                Some(v) => std::env::set_var("GIT_DIR", v),
                None => std::env::remove_var("GIT_DIR"),
            }
            match &self.prev_git_work_tree {
                Some(v) => std::env::set_var("GIT_WORK_TREE", v),
                None => std::env::remove_var("GIT_WORK_TREE"),
            }
            match &self.prev_git_common_dir {
                Some(v) => std::env::set_var("GIT_COMMON_DIR", v),
                None => std::env::remove_var("GIT_COMMON_DIR"),
            }
        }
    }
}

/// RAII guard that points `CRAB_CACHE_DIR` at the given tempdir for
/// the lifetime of the guard. Holds [`CACHE_DIR_MUTEX`] so concurrent
/// tests don't race on the env var.
pub struct CacheDirGuard {
    _lock: MutexGuard<'static, ()>,
    prev: Option<String>,
}

impl CacheDirGuard {
    pub fn new(path: &std::path::Path) -> Self {
        let lock = CACHE_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("CRAB_CACHE_DIR").ok();
        // SAFETY: access is serialised by CACHE_DIR_MUTEX.
        unsafe { std::env::set_var("CRAB_CACHE_DIR", path) };
        Self { _lock: lock, prev }
    }
}

impl Drop for CacheDirGuard {
    fn drop(&mut self) {
        // SAFETY: access is serialised by CACHE_DIR_MUTEX.
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("CRAB_CACHE_DIR", v) },
            None => unsafe { std::env::remove_var("CRAB_CACHE_DIR") },
        }
    }
}

pub struct TestGitRepo {
    _dir: tempfile::TempDir,
    pub git_dir: std::path::PathBuf,
    pub commit_sha: String,
}

impl TestGitRepo {
    fn create() -> Self {
        use std::process::Command;

        let tmp = tempfile::tempdir().expect("create temp dir for test git repo");
        let dir = tmp.path();

        let out = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(dir)
            .output()
            .expect("git init");
        assert!(
            out.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .expect("git config email");
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .expect("git config name");

        std::fs::write(dir.join("file.txt"), b"test content\n").expect("write file");
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(dir)
            .output()
            .expect("git add");
        let out = Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(dir)
            .output()
            .expect("git commit");
        assert!(
            out.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        Command::new("git")
            .args(["branch", "dev"])
            .current_dir(dir)
            .output()
            .expect("git branch dev");

        Command::new("git")
            .args(["tag", "v1.0"])
            .current_dir(dir)
            .output()
            .expect("git tag v1.0");

        let sha_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .expect("git rev-parse HEAD");
        let sha = String::from_utf8(sha_out.stdout).unwrap().trim().to_owned();

        let git_dir = dir.join(".git");

        Self {
            _dir: tmp,
            git_dir,
            commit_sha: sha,
        }
    }
}

/// RAII guard that sets `GIT_DIR` to the shared test repo and restores
/// it on drop. Holds the process-wide `GIT_DIR_MUTEX` to prevent races.
pub struct GitDirGuard {
    _lock: MutexGuard<'static, ()>,
    prev: Option<String>,
}

impl GitDirGuard {
    pub fn new() -> Self {
        let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("GIT_DIR").ok();
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        unsafe { std::env::set_var("GIT_DIR", &TEST_GIT_REPO.git_dir) };
        Self { _lock: lock, prev }
    }
}

impl Drop for GitDirGuard {
    fn drop(&mut self) {
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
            None => unsafe { std::env::remove_var("GIT_DIR") },
        }
    }
}
