use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};

use crab::git::worktree::WorktreeContext;

static GIT_ENV_LOCK: Mutex<()> = Mutex::new(());

fn git_env_lock() -> MutexGuard<'static, ()> {
    GIT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct GitEnvGuard {
    _lock: MutexGuard<'static, ()>,
    prev_git_dir: Option<std::ffi::OsString>,
    prev_git_work_tree: Option<std::ffi::OsString>,
    prev_git_common_dir: Option<std::ffi::OsString>,
}

impl GitEnvGuard {
    fn set(git_dir: &Path, work_tree: &Path, common_dir: &Path) -> Self {
        let lock = GIT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_git_dir = std::env::var_os("GIT_DIR");
        let prev_git_work_tree = std::env::var_os("GIT_WORK_TREE");
        let prev_git_common_dir = std::env::var_os("GIT_COMMON_DIR");
        // SAFETY: process environment access is serialized by GIT_ENV_LOCK.
        unsafe {
            std::env::set_var("GIT_DIR", git_dir);
            std::env::set_var("GIT_WORK_TREE", work_tree);
            std::env::set_var("GIT_COMMON_DIR", common_dir);
        }
        Self {
            _lock: lock,
            prev_git_dir,
            prev_git_work_tree,
            prev_git_common_dir,
        }
    }
}

impl Drop for GitEnvGuard {
    fn drop(&mut self) {
        // SAFETY: process environment access is serialized by GIT_ENV_LOCK.
        unsafe {
            match &self.prev_git_dir {
                Some(value) => std::env::set_var("GIT_DIR", value),
                None => std::env::remove_var("GIT_DIR"),
            }
            match &self.prev_git_work_tree {
                Some(value) => std::env::set_var("GIT_WORK_TREE", value),
                None => std::env::remove_var("GIT_WORK_TREE"),
            }
            match &self.prev_git_common_dir {
                Some(value) => std::env::set_var("GIT_COMMON_DIR", value),
                None => std::env::remove_var("GIT_COMMON_DIR"),
            }
        }
    }
}

fn run_git<I, S>(cwd: &Path, args: I) -> Option<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()
}

fn fixture() -> Option<tempfile::TempDir> {
    let tmp = tempfile::tempdir().ok()?;
    let repo = tmp.path().join("repo");
    if !Command::new("git")
        .args(["init", "-q", repo.to_str()?])
        .status()
        .ok()?
        .success()
    {
        return None;
    }
    run_git(&repo, ["config", "user.email", "worktree@crab.dev"])?;
    run_git(&repo, ["config", "user.name", "crab-worktree"])?;
    std::fs::write(repo.join("a.txt"), b"a\n").ok()?;
    run_git(&repo, ["add", "a.txt"])?;
    let commit = run_git(&repo, ["commit", "-qm", "init"])?;
    if !commit.status.success() {
        return None;
    }
    let linked = tmp.path().join("linked");
    let add = run_git(
        &repo,
        [
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str()?,
            "HEAD",
        ],
    )?;
    if !add.status.success() {
        return None;
    }
    Some(tmp)
}

fn linked_admin_dir(repo: &Path) -> std::path::PathBuf {
    repo.join(".git").join("worktrees").join("linked")
}

#[test]
fn context_resolves_main_worktree() {
    let _env_lock = git_env_lock();
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");

    let ctx = WorktreeContext::resolve_from(&repo).expect("main context");
    assert_eq!(ctx.identity, "main");
    assert_eq!(ctx.current_worktree_root, repo.canonicalize().unwrap());
    assert_eq!(ctx.main_worktree_root, repo.canonicalize().unwrap());
    assert_eq!(
        ctx.common_git_dir,
        repo.join(".git").canonicalize().unwrap()
    );
    assert_eq!(ctx.per_worktree_git_dir, ctx.common_git_dir);
    assert_eq!(ctx.shared_crab_dir, ctx.main_worktree_root.join(".crab"));
    assert_eq!(
        ctx.per_worktree_crab_dir,
        ctx.shared_crab_dir.join("worktrees").join("main")
    );
    assert_eq!(
        crab::speculation::access_db::path_for_context(&ctx),
        ctx.per_worktree_crab_dir.join("access.db")
    );
    assert_eq!(ctx.index_path(), ctx.common_git_dir.join("index"));
    assert_eq!(ctx.objects_dir(), ctx.common_git_dir.join("objects"));
    assert_eq!(
        ctx.lfs_objects_dir(),
        ctx.common_git_dir.join("lfs").join("objects")
    );
    assert_eq!(
        ctx.shared_staging_dir(),
        ctx.shared_crab_dir.join("staging")
    );
}

#[test]
fn context_resolves_absolute_worktree_links() {
    let _env_lock = git_env_lock();
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo").canonicalize().unwrap();
    let linked = tmp.path().join("linked").canonicalize().unwrap();
    let git_file = std::fs::read_to_string(linked.join(".git")).expect("linked .git file");

    assert!(git_file.trim_start().starts_with("gitdir: /"));
    let ctx = WorktreeContext::resolve_from(&linked).expect("linked context");
    assert_eq!(ctx.identity, "linked");
    assert_eq!(ctx.current_worktree_root, linked);
    assert_eq!(ctx.main_worktree_root, repo);
}

#[test]
fn context_resolves_relative_worktree_links() {
    let _env_lock = git_env_lock();
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo").canonicalize().unwrap();
    let linked = tmp.path().join("linked").canonicalize().unwrap();
    let admin_dir = linked_admin_dir(&repo);

    std::fs::write(
        linked.join(".git"),
        "gitdir: ../repo/.git/worktrees/linked\n",
    )
    .expect("write relative .git file");
    std::fs::write(admin_dir.join("gitdir"), "../../../../linked/.git\n")
        .expect("write relative gitdir file");

    let git = run_git(&linked, ["rev-parse", "--show-toplevel"]).expect("git rev-parse");
    assert!(
        git.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&git.stderr)
    );

    let ctx = WorktreeContext::resolve_from(&linked).expect("relative linked context");
    assert_eq!(ctx.identity, "linked");
    assert_eq!(ctx.current_worktree_root, linked);
    assert_eq!(ctx.main_worktree_root, repo);
    assert_eq!(ctx.common_git_dir, ctx.main_worktree_root.join(".git"));
}

#[test]
fn context_resolves_linked_worktree_from_nested_directory() {
    let _env_lock = git_env_lock();
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo").canonicalize().unwrap();
    let linked = tmp.path().join("linked");
    let nested = linked.join("src/deep");
    std::fs::create_dir_all(&nested).expect("nested dir");

    let ctx = WorktreeContext::resolve_from(&nested).expect("linked context");
    assert_eq!(ctx.identity, "linked");
    assert_eq!(ctx.current_worktree_root, linked.canonicalize().unwrap());
    assert_eq!(ctx.main_worktree_root, repo);
    assert_eq!(ctx.common_git_dir, ctx.main_worktree_root.join(".git"));
    assert!(ctx.per_worktree_git_dir.ends_with(".git/worktrees/linked"));
    assert_eq!(
        ctx.per_worktree_crab_dir,
        ctx.shared_crab_dir.join("worktrees").join("linked")
    );
    assert_eq!(
        crab::speculation::access_db::path_for_context(&ctx),
        ctx.per_worktree_crab_dir.join("access.db")
    );
    assert_eq!(ctx.index_path(), ctx.per_worktree_git_dir.join("index"));
    assert_eq!(ctx.objects_dir(), ctx.common_git_dir.join("objects"));
    assert_eq!(
        ctx.lfs_objects_dir(),
        ctx.common_git_dir.join("lfs").join("objects")
    );
    assert_eq!(
        ctx.shared_staging_dir(),
        ctx.shared_crab_dir.join("staging")
    );
}

#[test]
fn context_honors_git_dir_and_work_tree_environment() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo").canonicalize().unwrap();
    let linked = tmp.path().join("linked").canonicalize().unwrap();
    let admin_dir = linked_admin_dir(&repo).canonicalize().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside dir");

    let _env = GitEnvGuard::set(&admin_dir, &linked, &repo.join(".git"));
    let ctx = WorktreeContext::resolve_from(&outside).expect("env directed context");

    assert_eq!(ctx.identity, "linked");
    assert_eq!(ctx.current_worktree_root, linked);
    assert_eq!(ctx.main_worktree_root, repo);
    assert_eq!(ctx.per_worktree_git_dir, admin_dir);
}

#[test]
fn context_path_resolution_ignores_git_environment() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo").canonicalize().unwrap();
    let linked = tmp.path().join("linked").canonicalize().unwrap();
    let admin_dir = linked_admin_dir(&repo).canonicalize().unwrap();

    let _env = GitEnvGuard::set(&admin_dir, &linked, &repo.join(".git"));

    let env_ctx = WorktreeContext::resolve_from(&repo).expect("env directed context");
    assert_eq!(env_ctx.identity, "linked");
    assert_eq!(env_ctx.current_worktree_root, linked);

    let path_ctx = WorktreeContext::resolve_from_path(&repo).expect("path context");
    assert_eq!(path_ctx.identity, "main");
    assert_eq!(path_ctx.current_worktree_root, repo);
}

#[test]
fn context_preserves_per_worktree_git_config_location() {
    let _env_lock = git_env_lock();
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo").canonicalize().unwrap();
    let linked = tmp.path().join("linked").canonicalize().unwrap();

    let enable = run_git(&repo, ["config", "extensions.worktreeConfig", "true"])
        .expect("enable worktree config");
    if !enable.status.success() {
        eprintln!("SKIP: failed to enable worktree config");
        return;
    }
    let set = run_git(
        &linked,
        ["config", "--worktree", "crab.context-test", "linked"],
    )
    .expect("set worktree config");
    if !set.status.success() {
        eprintln!("SKIP: failed to write worktree config");
        return;
    }

    let config_path =
        run_git(&linked, ["rev-parse", "--git-path", "config.worktree"]).expect("config path");
    assert!(config_path.status.success());
    let config_path = String::from_utf8_lossy(&config_path.stdout);
    let config_path = std::path::PathBuf::from(config_path.trim());

    let ctx = WorktreeContext::resolve_from(&linked).expect("linked context");
    assert_eq!(
        config_path,
        ctx.per_worktree_git_dir.join("config.worktree")
    );
    assert_ne!(config_path, ctx.common_git_dir.join("config.worktree"));
}

#[test]
fn context_identity_survives_branch_changes_and_detached_head() {
    let _env_lock = git_env_lock();
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let linked = tmp.path().join("linked").canonicalize().unwrap();

    let checkout = run_git(&linked, ["checkout", "-q", "-b", "feature"]).expect("checkout");
    if !checkout.status.success() {
        eprintln!("SKIP: failed to create linked branch");
        return;
    }
    let branch_ctx = WorktreeContext::resolve_from(&linked).expect("branch context");
    assert_eq!(branch_ctx.identity, "linked");

    let detach = run_git(&linked, ["checkout", "-q", "--detach", "HEAD"]).expect("detach");
    if !detach.status.success() {
        eprintln!("SKIP: failed to detach linked worktree");
        return;
    }
    let detached_ctx = WorktreeContext::resolve_from(&linked).expect("detached context");
    assert_eq!(detached_ctx.identity, "linked");
}

#[test]
fn context_rejects_non_repository_paths() {
    let _env_lock = git_env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let err = WorktreeContext::resolve_from(tmp.path()).expect_err("non-repo should fail");
    assert!(err.to_string().contains("failed to discover Git worktree"));
}

#[test]
fn context_rejects_bare_repository_as_worktree() {
    let _env_lock = git_env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare.git");
    let status = Command::new("git")
        .args(["init", "--bare", "-q", bare.to_str().unwrap()])
        .status()
        .expect("git init bare");
    if !status.success() {
        eprintln!("SKIP: git unavailable or bare init failed");
        return;
    }

    let err = WorktreeContext::resolve_from(&bare).expect_err("bare repo should fail");
    assert!(err.to_string().contains("not inside a Git working tree"));
}
