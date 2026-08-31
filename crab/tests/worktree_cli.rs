use std::process::{Command, Output};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

fn crab_bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

fn run_git<I, S>(cwd: &std::path::Path, args: I) -> Option<Output>
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

fn git_worktree_add_help_contains(option: &str) -> bool {
    git_worktree_subcommand_help_contains("add", option)
}

fn git_worktree_subcommand_help_contains(subcommand: &str, option: &str) -> bool {
    let Ok(output) = Command::new("git")
        .args(["worktree", subcommand, "-h"])
        .output()
    else {
        return false;
    };
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    help.contains(option)
}

fn worktree_record_paths(repo: &std::path::Path) -> Vec<std::path::PathBuf> {
    let list = run_git(repo, ["worktree", "list", "--porcelain", "-z"]).expect("git list");
    let records =
        crab::git::worktree::parse_worktree_list_porcelain(&list.stdout, true).expect("parse");
    records
        .into_iter()
        .filter_map(|record| std::path::Path::new(&record.path).canonicalize().ok())
        .collect()
}

fn worktree_record_for_path(
    repo: &std::path::Path,
    path: &std::path::Path,
) -> Option<crab::git::worktree::GitWorktreeRecord> {
    let list = run_git(repo, ["worktree", "list", "--porcelain", "-z"])?;
    let records = crab::git::worktree::parse_worktree_list_porcelain(&list.stdout, true).ok()?;
    let path = crab::git::worktree::normalize_identity_path(path);
    records.into_iter().find(|record| {
        crab::git::worktree::normalize_identity_path(std::path::Path::new(&record.path)) == path
    })
}

fn hydration_policy_for(worktree: &std::path::Path) -> toml::Value {
    let policy =
        std::fs::read_to_string(hydration_policy_path(worktree)).expect("hydration policy");
    toml::from_str(&policy).expect("parse hydration policy")
}

fn hydration_policy_path(worktree: &std::path::Path) -> std::path::PathBuf {
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(worktree).expect("worktree ctx");
    ctx.per_worktree_crab_dir.join("hydration-policy.toml")
}

fn selector_table(policy: &toml::Value) -> &toml::map::Map<String, toml::Value> {
    policy
        .get("selector")
        .and_then(toml::Value::as_table)
        .expect("selector table")
}

fn toml_string<'a>(value: &'a toml::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(toml::Value::as_str)
}

fn json_envelope(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json envelope")
}

fn repo_fixture() -> Option<tempfile::TempDir> {
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
    Some(tmp)
}

fn fixture() -> Option<tempfile::TempDir> {
    let tmp = repo_fixture()?;
    let repo = tmp.path().join("repo");
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

#[test]
fn worktree_mutating_subcommands_match_git_state_transitions() {
    let Some(git_tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let Some(crab_tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let git_repo = git_tmp.path().join("repo");
    let crab_repo = crab_tmp.path().join("repo");
    let git_linked = git_tmp.path().join("linked");
    let crab_linked = crab_tmp.path().join("linked");

    let git_add = run_git(
        &git_repo,
        [
            "worktree",
            "add",
            "-q",
            "--detach",
            git_linked.to_str().unwrap(),
            "HEAD",
        ],
    )
    .expect("git add");
    let crab_add = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            crab_linked.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&crab_repo)
        .output()
        .expect("crab add");
    assert_eq!(git_add.status.success(), crab_add.status.success());
    assert_eq!(git_add.stdout, crab_add.stdout);
    assert_eq!(git_add.stderr, crab_add.stderr);
    let git_record = worktree_record_for_path(&git_repo, &git_linked).expect("git linked record");
    let crab_record =
        worktree_record_for_path(&crab_repo, &crab_linked).expect("crab linked record");
    assert_eq!(git_record.detached, crab_record.detached);
    assert_eq!(git_record.branch.is_some(), crab_record.branch.is_some());
    assert_eq!(git_record.locked, crab_record.locked);
    assert_eq!(git_record.head.is_some(), crab_record.head.is_some());

    let git_lock = run_git(
        &git_repo,
        [
            "worktree",
            "lock",
            "--reason",
            "parity",
            git_linked.to_str().unwrap(),
        ],
    )
    .expect("git lock");
    let crab_lock = Command::new(crab_bin())
        .args([
            "worktree",
            "lock",
            "--reason",
            "parity",
            crab_linked.to_str().unwrap(),
        ])
        .current_dir(&crab_repo)
        .output()
        .expect("crab lock");
    assert_eq!(git_lock.status.success(), crab_lock.status.success());
    assert_eq!(git_lock.stdout, crab_lock.stdout);
    assert_eq!(git_lock.stderr, crab_lock.stderr);
    let git_record = worktree_record_for_path(&git_repo, &git_linked).expect("git locked record");
    let crab_record =
        worktree_record_for_path(&crab_repo, &crab_linked).expect("crab locked record");
    assert_eq!(git_record.locked, crab_record.locked);
    assert_eq!(git_record.lock_reason, crab_record.lock_reason);

    let git_unlock = run_git(
        &git_repo,
        ["worktree", "unlock", git_linked.to_str().unwrap()],
    )
    .expect("git unlock");
    let crab_unlock = Command::new(crab_bin())
        .args(["worktree", "unlock", crab_linked.to_str().unwrap()])
        .current_dir(&crab_repo)
        .output()
        .expect("crab unlock");
    assert_eq!(git_unlock.status.success(), crab_unlock.status.success());
    assert_eq!(git_unlock.stdout, crab_unlock.stdout);
    assert_eq!(git_unlock.stderr, crab_unlock.stderr);

    let git_moved = git_tmp.path().join("moved");
    let crab_moved = crab_tmp.path().join("moved");
    let git_move = run_git(
        &git_repo,
        [
            "worktree",
            "move",
            git_linked.to_str().unwrap(),
            git_moved.to_str().unwrap(),
        ],
    )
    .expect("git move");
    let crab_move = Command::new(crab_bin())
        .args([
            "worktree",
            "move",
            crab_linked.to_str().unwrap(),
            crab_moved.to_str().unwrap(),
        ])
        .current_dir(&crab_repo)
        .output()
        .expect("crab move");
    assert_eq!(git_move.status.success(), crab_move.status.success());
    assert_eq!(git_move.stdout, crab_move.stdout);
    assert_eq!(git_move.stderr, crab_move.stderr);
    assert_eq!(
        worktree_record_for_path(&git_repo, &git_linked).is_some(),
        worktree_record_for_path(&crab_repo, &crab_linked).is_some()
    );
    assert_eq!(
        worktree_record_for_path(&git_repo, &git_moved).is_some(),
        worktree_record_for_path(&crab_repo, &crab_moved).is_some()
    );

    let git_remove = run_git(
        &git_repo,
        ["worktree", "remove", "--force", git_moved.to_str().unwrap()],
    )
    .expect("git remove");
    let crab_remove = Command::new(crab_bin())
        .args([
            "worktree",
            "remove",
            "--force",
            crab_moved.to_str().unwrap(),
        ])
        .current_dir(&crab_repo)
        .output()
        .expect("crab remove");
    assert_eq!(git_remove.status.success(), crab_remove.status.success());
    assert_eq!(git_remove.stdout, crab_remove.stdout);
    assert_eq!(git_remove.stderr, crab_remove.stderr);
    assert_eq!(git_moved.exists(), crab_moved.exists());
    assert_eq!(
        worktree_record_for_path(&git_repo, &git_moved).is_some(),
        worktree_record_for_path(&crab_repo, &crab_moved).is_some()
    );
}

#[test]
fn worktree_lock_and_unlock_delegate_to_git() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");

    let lock = Command::new(crab_bin())
        .args([
            "worktree",
            "lock",
            "--reason",
            "testing lock",
            linked.to_str().unwrap(),
        ])
        .current_dir(&repo)
        .output()
        .expect("crab lock");
    assert!(
        lock.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&lock.stderr)
    );
    let list = run_git(&repo, ["worktree", "list", "--porcelain", "-z"]).expect("git list");
    let records =
        crab::git::worktree::parse_worktree_list_porcelain(&list.stdout, true).expect("parse");
    let linked_canonical = linked.canonicalize().unwrap();
    let linked_record = records
        .iter()
        .find(|record| {
            std::path::Path::new(&record.path)
                .canonicalize()
                .map(|path| path == linked_canonical)
                .unwrap_or(false)
        })
        .expect("linked record");
    assert!(linked_record.locked);
    assert_eq!(linked_record.lock_reason.as_deref(), Some("testing lock"));

    let unlock = Command::new(crab_bin())
        .args(["worktree", "unlock", linked.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab unlock");
    assert!(
        unlock.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&unlock.stderr)
    );
    let list = run_git(&repo, ["worktree", "list", "--porcelain", "-z"]).expect("git list");
    let records =
        crab::git::worktree::parse_worktree_list_porcelain(&list.stdout, true).expect("parse");
    let linked_record = records
        .iter()
        .find(|record| {
            std::path::Path::new(&record.path)
                .canonicalize()
                .map(|path| path == linked_canonical)
                .unwrap_or(false)
        })
        .expect("linked record");
    assert!(!linked_record.locked);
}

#[test]
fn worktree_move_and_remove_delegate_to_git() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let moved = tmp.path().join("moved");

    let move_output = Command::new(crab_bin())
        .args([
            "worktree",
            "move",
            linked.to_str().unwrap(),
            moved.to_str().unwrap(),
        ])
        .current_dir(&repo)
        .output()
        .expect("crab move");
    assert!(
        move_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&move_output.stderr)
    );
    assert!(!linked.exists());
    assert!(moved.join(".git").exists());
    assert!(worktree_record_paths(&repo).contains(&moved.canonicalize().unwrap()));
    let moved_ctx = crab::git::worktree::WorktreeContext::resolve_from(&moved).expect("moved ctx");
    assert_eq!(moved_ctx.identity, "linked");

    let remove_output = Command::new(crab_bin())
        .args(["worktree", "remove", "--force", moved.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab remove");
    assert!(
        remove_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&remove_output.stderr)
    );
    assert!(!moved.exists());
    assert!(!worktree_record_paths(&repo).contains(&moved));
}

#[test]
fn worktree_remove_deletes_unlocked_per_worktree_state_after_git_removes_record() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(&linked).expect("linked ctx");
    let shared_marker = ctx.shared_crab_dir.join("staging").join("shared.txt");
    std::fs::create_dir_all(shared_marker.parent().unwrap()).expect("shared marker parent");
    std::fs::write(&shared_marker, b"shared").expect("shared marker");
    std::fs::create_dir_all(&ctx.per_worktree_crab_dir).expect("state dir");
    std::fs::write(ctx.per_worktree_crab_dir.join("sentinel"), b"state").expect("state marker");

    let remove_output = Command::new(crab_bin())
        .args(["worktree", "remove", "--force", linked.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab remove");

    assert!(
        remove_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&remove_output.stderr)
    );
    assert!(!ctx.per_worktree_crab_dir.exists());
    assert!(shared_marker.exists());
}

#[cfg(unix)]
#[test]
fn worktree_remove_reports_locked_per_worktree_state_and_leaves_it_in_place() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(&linked).expect("linked ctx");
    std::fs::create_dir_all(&ctx.per_worktree_crab_dir).expect("state dir");
    std::fs::write(ctx.per_worktree_crab_dir.join("sentinel"), b"state").expect("state marker");
    let lock_path = ctx.per_worktree_crab_dir.join("state.lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("lock file");
    // SAFETY: the fd belongs to `lock_file`, which stays open until
    // after the child process attempts cleanup.
    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "failed to lock state file");

    let remove_output = Command::new(crab_bin())
        .args(["worktree", "remove", "--force", linked.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab remove");

    assert!(
        remove_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&remove_output.stderr)
    );
    assert!(ctx.per_worktree_crab_dir.join("sentinel").exists());
    let stderr = String::from_utf8_lossy(&remove_output.stderr);
    assert!(
        stderr.contains("skipped Crab state cleanup"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("locked"), "stderr: {stderr}");
    drop(lock_file);
}

#[test]
fn worktree_remove_preserves_git_force_count_for_locked_worktree() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let lock = run_git(&repo, ["worktree", "lock", linked.to_str().unwrap()]).expect("git lock");
    if !lock.status.success() {
        eprintln!("SKIP: failed to lock worktree");
        return;
    }

    let one_force = Command::new(crab_bin())
        .args(["worktree", "remove", "--force", linked.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab remove one force");
    assert!(!one_force.status.success());
    let stderr = String::from_utf8_lossy(&one_force.stderr);
    assert!(!stderr.contains("filters disabled"), "stderr: {stderr}");
    assert!(stderr.contains("git worktree remove"), "stderr: {stderr}");
    assert!(stderr.contains("fatal:"), "stderr: {stderr}");
    assert!(linked.exists());

    let two_force = Command::new(crab_bin())
        .args([
            "worktree",
            "remove",
            "--force",
            "--force",
            linked.to_str().unwrap(),
        ])
        .current_dir(&repo)
        .output()
        .expect("crab remove two force");
    assert!(
        two_force.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&two_force.stderr)
    );
    assert!(!linked.exists());
}

#[test]
fn worktree_remove_preserves_git_force_count_for_dirty_worktree() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    std::fs::write(linked.join("dirty.txt"), b"dirty").expect("dirty file");

    let no_force = Command::new(crab_bin())
        .args(["worktree", "remove", linked.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab remove no force");
    assert!(!no_force.status.success());
    let stderr = String::from_utf8_lossy(&no_force.stderr);
    assert!(!stderr.contains("filters disabled"), "stderr: {stderr}");
    assert!(linked.exists());

    let one_force = Command::new(crab_bin())
        .args(["worktree", "remove", "--force", linked.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab remove one force");
    assert!(
        one_force.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&one_force.stderr)
    );
    assert!(!linked.exists());
}

#[test]
fn worktree_remove_submodule_failure_does_not_disable_filters() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let submodule_src = tmp.path().join("submodule-src");
    if !Command::new("git")
        .args(["init", "-q", submodule_src.to_str().unwrap()])
        .status()
        .expect("git init submodule")
        .success()
    {
        eprintln!("SKIP: failed to initialize submodule source");
        return;
    }
    run_git(
        &submodule_src,
        ["config", "user.email", "worktree@crab.dev"],
    )
    .expect("submodule email");
    run_git(&submodule_src, ["config", "user.name", "crab-worktree"]).expect("submodule user");
    std::fs::write(submodule_src.join("sub.txt"), b"sub\n").expect("submodule file");
    run_git(&submodule_src, ["add", "sub.txt"]).expect("submodule add file");
    let commit =
        run_git(&submodule_src, ["commit", "-qm", "submodule init"]).expect("submodule commit");
    if !commit.status.success() {
        eprintln!("SKIP: failed to commit submodule source");
        return;
    }

    let add_submodule = run_git(
        &repo,
        [
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            submodule_src.to_str().unwrap(),
            "deps/sub",
        ],
    )
    .expect("submodule add");
    if !add_submodule.status.success() {
        eprintln!(
            "SKIP: failed to add local submodule: {}",
            String::from_utf8_lossy(&add_submodule.stderr)
        );
        return;
    }
    let commit = run_git(&repo, ["commit", "-qm", "add submodule"]).expect("commit submodule");
    if !commit.status.success() {
        eprintln!(
            "SKIP: failed to commit submodule: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
        return;
    }
    let head = run_git(&repo, ["rev-parse", "HEAD"]).expect("rev-parse head");
    if !head.status.success() {
        eprintln!("SKIP: failed to resolve submodule commit");
        return;
    }
    let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    let checkout = run_git(&linked, ["checkout", "-q", head.as_str()]).expect("linked checkout");
    if !checkout.status.success() {
        eprintln!(
            "SKIP: failed to checkout submodule commit in linked worktree: {}",
            String::from_utf8_lossy(&checkout.stderr)
        );
        return;
    }

    let update = run_git(
        &linked,
        [
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ],
    )
    .expect("submodule update");
    if !update.status.success() {
        eprintln!(
            "SKIP: failed to initialize linked submodule: {}",
            String::from_utf8_lossy(&update.stderr)
        );
        return;
    }

    let output = Command::new(crab_bin())
        .args(["worktree", "remove", linked.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab remove submodule worktree");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("filters disabled"), "stderr: {stderr}");
    assert!(stderr.contains("submodule"), "stderr: {stderr}");
    assert!(linked.exists());
}

#[test]
fn worktree_remove_unrelated_git_failure_does_not_disable_filters() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let not_worktree = tmp.path().join("not-a-worktree");
    std::fs::create_dir(&not_worktree).expect("not worktree dir");

    let output = Command::new(crab_bin())
        .args(["worktree", "remove", not_worktree.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab remove non-worktree");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("filters disabled"), "stderr: {stderr}");
    assert!(not_worktree.exists());
}

#[test]
fn worktree_remove_preserves_git_behavior_for_missing_worktree() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    std::fs::remove_dir_all(&linked).expect("remove linked path");

    let output = Command::new(crab_bin())
        .args(["worktree", "remove", linked.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab remove missing");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let list = run_git(&repo, ["worktree", "list", "--porcelain", "-z"]).expect("git list");
    let records =
        crab::git::worktree::parse_worktree_list_porcelain(&list.stdout, true).expect("parse");
    let linked = crab::git::worktree::normalize_identity_path(&linked);
    assert!(
        !records
            .iter()
            .any(
                |record| crab::git::worktree::normalize_identity_path(std::path::Path::new(
                    &record.path
                )) == linked
            )
    );
}

#[test]
fn worktree_prune_dry_run_matches_git_for_missing_worktree() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    std::fs::remove_dir_all(&linked).expect("remove linked worktree path");

    let git = run_git(&repo, ["worktree", "prune", "--dry-run", "--verbose"]).expect("git prune");
    let crab = Command::new(crab_bin())
        .args(["worktree", "prune", "--dry-run", "--verbose"])
        .current_dir(&repo)
        .output()
        .expect("crab prune");

    assert_eq!(git.status.success(), crab.status.success());
    assert_eq!(git.stdout, crab.stdout);
    assert_eq!(git.stderr, crab.stderr);
}

#[test]
fn worktree_prune_deletes_unlocked_state_after_git_prunes_record() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(&linked).expect("linked ctx");
    std::fs::create_dir_all(&ctx.per_worktree_crab_dir).expect("state dir");
    std::fs::write(ctx.per_worktree_crab_dir.join("sentinel"), b"state").expect("state marker");
    std::fs::remove_dir_all(&linked).expect("remove linked worktree path");

    let prune = Command::new(crab_bin())
        .args(["worktree", "prune", "--verbose"])
        .current_dir(&repo)
        .output()
        .expect("crab prune");

    assert!(
        prune.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&prune.stderr)
    );
    assert!(!ctx.per_worktree_crab_dir.exists());
}

#[cfg(unix)]
#[test]
fn worktree_prune_reports_locked_per_worktree_state_and_leaves_it_in_place() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(&linked).expect("linked ctx");
    std::fs::create_dir_all(&ctx.per_worktree_crab_dir).expect("state dir");
    std::fs::write(ctx.per_worktree_crab_dir.join("sentinel"), b"state").expect("state marker");
    let lock_path = ctx.per_worktree_crab_dir.join("state.lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("lock file");
    // SAFETY: the fd belongs to `lock_file`, which stays open until
    // after the child process attempts cleanup.
    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "failed to lock state file");
    std::fs::remove_dir_all(&linked).expect("remove linked worktree path");

    let prune = Command::new(crab_bin())
        .args(["worktree", "prune", "--verbose"])
        .current_dir(&repo)
        .output()
        .expect("crab prune");

    assert!(
        prune.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&prune.stderr)
    );
    assert!(ctx.per_worktree_crab_dir.join("sentinel").exists());
    let stderr = String::from_utf8_lossy(&prune.stderr);
    assert!(
        stderr.contains("skipped Crab state cleanup"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("locked"), "stderr: {stderr}");
    drop(lock_file);
}

#[test]
fn worktree_repair_delegates_to_git() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");

    let output = Command::new(crab_bin())
        .args(["worktree", "repair", linked.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab repair");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn worktree_repair_preserves_existing_per_worktree_state_identity() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let moved = tmp.path().join("manual-moved");
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(&linked).expect("linked ctx");
    std::fs::create_dir_all(&ctx.per_worktree_crab_dir).expect("state dir");
    let sentinel = ctx.per_worktree_crab_dir.join("sentinel");
    std::fs::write(&sentinel, b"state").expect("state marker");
    std::fs::rename(&linked, &moved).expect("move linked worktree outside Git");

    let output = Command::new(crab_bin())
        .args(["worktree", "repair", moved.to_str().unwrap()])
        .current_dir(&repo)
        .output()
        .expect("crab repair");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let moved_ctx = crab::git::worktree::WorktreeContext::resolve_from(&moved).expect("moved ctx");
    assert_eq!(moved_ctx.identity, ctx.identity);
    assert!(sentinel.exists());
}

#[test]
fn worktree_repair_rejects_version_gated_options_before_mutation() {
    let unsupported = ["--relative-paths", "--no-relative-paths"]
        .into_iter()
        .filter(|option| !git_worktree_subcommand_help_contains("repair", option))
        .collect::<Vec<_>>();
    if unsupported.is_empty() {
        eprintln!("SKIP: installed Git supports tracked version-gated repair options");
        return;
    }
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");

    for option in unsupported {
        let output = Command::new(crab_bin())
            .args(["worktree", "repair", option, linked.to_str().unwrap()])
            .current_dir(&repo)
            .output()
            .expect("crab repair");

        assert!(!output.status.success());
        assert!(linked.exists());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(option), "stderr: {stderr}");
        assert!(stderr.contains("crab worktree repair"), "stderr: {stderr}");
        assert!(stderr.contains("not supported"), "stderr: {stderr}");
    }
}

#[test]
fn worktree_add_uses_user_supplied_path() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let requested = tmp.path().join("custom-location");

    let output = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            requested.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requested.join(".git").exists());

    let list = run_git(&repo, ["worktree", "list", "--porcelain", "-z"]).expect("git list");
    let records =
        crab::git::worktree::parse_worktree_list_porcelain(&list.stdout, true).expect("parse");
    let requested = requested.canonicalize().unwrap();
    assert!(records.iter().any(|record| {
        std::path::Path::new(&record.path)
            .canonicalize()
            .map(|path| path == requested)
            .unwrap_or(false)
    }));
}

#[test]
fn worktree_add_no_checkout_delegates_without_materializing_files() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let requested = tmp.path().join("no-checkout-location");

    let output = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            "--no-checkout",
            requested.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requested.join(".git").exists());
    assert!(!requested.join("a.txt").exists());
    assert!(!hydration_policy_path(&requested).exists());
}

#[test]
fn crab_add_from_linked_worktree_uses_current_index_and_shared_staging() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    std::fs::write(
        repo.join(".gitattributes"),
        b"*.bin filter=crab diff=crab merge=crab -text\n",
    )
    .expect("attributes");
    run_git(&repo, ["add", ".gitattributes"]).expect("git add attributes");
    let commit = run_git(&repo, ["commit", "-qm", "track crab files"]).expect("commit");
    if !commit.status.success() {
        eprintln!("SKIP: failed to commit attributes");
        return;
    }

    let linked = tmp.path().join("linked");
    let add_worktree = run_git(
        &repo,
        [
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    )
    .expect("worktree add");
    if !add_worktree.status.success() {
        eprintln!("SKIP: failed to add linked worktree");
        return;
    }

    std::fs::write(linked.join("model.bin"), b"linked worktree bytes").expect("model");
    let output = Command::new(crab_bin())
        .args(["add", "model.bin"])
        .current_dir(&linked)
        .output()
        .expect("crab add output");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let linked_blob = run_git(&linked, ["show", ":model.bin"]).expect("linked index blob");
    assert!(
        linked_blob.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&linked_blob.stderr)
    );
    let pointer_text = String::from_utf8_lossy(&linked_blob.stdout);
    assert!(pointer_text.contains("version https://crab.dev/spec/v1"));

    let main_lookup =
        run_git(&repo, ["ls-files", "--error-unmatch", "model.bin"]).expect("main index lookup");
    assert!(!main_lookup.status.success());

    assert!(repo.join(".crab").join("staging").exists());
    assert!(!linked.join(".crab").join("staging").exists());
}

#[test]
fn worktree_add_no_checkout_hydrate_full_persists_pending_policy_without_checkout() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let requested = tmp.path().join("no-checkout-full");

    let output = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            "--no-checkout",
            "--hydrate=full",
            requested.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requested.join(".git").exists());
    assert!(!requested.join("a.txt").exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("hydration deferred"), "stderr: {stderr}");

    let policy = hydration_policy_for(&requested);
    assert_eq!(toml_string(&policy, "source"), Some("explicit"));
    assert_eq!(toml_string(&policy, "status"), Some("pending"));
    assert_eq!(toml_string(&policy, "mode"), Some("full"));
    assert_eq!(
        policy
            .get("checkout_suppressed")
            .and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        selector_table(&policy)
            .get("kind")
            .and_then(toml::Value::as_str),
        Some("all")
    );
}

#[test]
fn worktree_add_no_checkout_non_materializing_policies_are_not_pending() {
    for policy_name in ["lazy", "pointer-only"] {
        let Some(tmp) = repo_fixture() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        let repo = tmp.path().join("repo");
        let requested = tmp.path().join(format!("no-checkout-{policy_name}"));

        let output = Command::new(crab_bin())
            .args([
                "worktree",
                "add",
                "--quiet",
                "--detach",
                "--no-checkout",
                &format!("--hydrate={policy_name}"),
                requested.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo)
            .output()
            .expect("crab output");

        assert!(
            output.status.success(),
            "policy {policy_name} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(requested.join(".git").exists());
        assert!(!requested.join("a.txt").exists());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no pending hydration"),
            "policy {policy_name} stderr: {stderr}"
        );

        let policy = hydration_policy_for(&requested);
        assert_eq!(toml_string(&policy, "source"), Some("explicit"));
        assert_eq!(toml_string(&policy, "status"), Some("applied"));
        assert_eq!(toml_string(&policy, "mode"), Some(policy_name));
        assert_eq!(
            policy
                .get("checkout_suppressed")
                .and_then(toml::Value::as_bool),
            Some(true)
        );
    }
}

#[test]
fn worktree_add_plain_no_checkout_uses_clone_default_policy_without_git_config_mutation() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    std::fs::write(
        repo.join("crab.toml"),
        b"[remote]\nurl = \"crab://bucket/repo\"\n\n[hydrate]\ndefault = \"eager\"\n",
    )
    .expect("project config");
    let config_enable =
        run_git(&repo, ["config", "extensions.worktreeConfig", "true"]).expect("config");
    if !config_enable.status.success() {
        eprintln!("SKIP: installed Git cannot enable worktreeConfig");
        return;
    }

    let requested = tmp.path().join("clone-default-no-checkout");
    let output = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            "--no-checkout",
            requested.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let policy = hydration_policy_for(&requested);
    assert_eq!(toml_string(&policy, "source"), Some("clone-defaults"));
    assert_eq!(toml_string(&policy, "status"), Some("pending"));
    assert_eq!(toml_string(&policy, "mode"), Some("full"));
    assert_eq!(
        selector_table(&policy)
            .get("kind")
            .and_then(toml::Value::as_str),
        Some("all")
    );

    let git_path = run_git(&requested, ["rev-parse", "--git-path", "config.worktree"])
        .expect("git config.worktree path");
    assert!(
        git_path.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&git_path.stderr)
    );
    let config_path_text = String::from_utf8_lossy(&git_path.stdout);
    let config_path = std::path::PathBuf::from(config_path_text.trim());
    let config_path = if config_path.is_absolute() {
        config_path
    } else {
        requested.join(config_path)
    };
    let config_body = std::fs::read_to_string(&config_path).unwrap_or_default();
    assert!(!config_body.contains("hydrate"), "{config_body}");
    assert!(!config_body.contains("prefetch"), "{config_body}");
    assert!(!config_body.contains("crab.worktree"), "{config_body}");
}

#[test]
fn worktree_add_checked_out_reuses_clone_default_hydration_policy() {
    let cases = [
        (
            "eager",
            "[remote]\nurl = \"crab://bucket/repo\"\n\n[hydrate]\ndefault = \"eager\"\n",
            "full",
            "all",
        ),
        (
            "auto-patterns",
            "[remote]\nurl = \"crab://bucket/repo\"\n\n[hydrate]\ndefault = \"lazy\"\nauto_patterns = [\"*.bin\"]\n",
            "selective",
            "patterns",
        ),
    ];

    for (name, config, expected_mode, expected_selector) in cases {
        let Some(tmp) = repo_fixture() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        let repo = tmp.path().join("repo");
        std::fs::write(repo.join("crab.toml"), config).expect("project config");

        let requested = tmp.path().join(format!("clone-default-{name}"));
        let output = Command::new(crab_bin())
            .args([
                "worktree",
                "add",
                "--quiet",
                "--detach",
                requested.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo)
            .output()
            .expect("crab output");

        assert!(
            output.status.success(),
            "case {name} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("hydrating selected Crab pointer files"),
            "case {name} stderr: {stderr}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("No pointer files match"),
            "case {name} stdout: {stdout}"
        );
        let policy = hydration_policy_for(&requested);
        assert_eq!(toml_string(&policy, "source"), Some("clone-defaults"));
        assert_eq!(toml_string(&policy, "status"), Some("applied"));
        assert_eq!(toml_string(&policy, "mode"), Some(expected_mode));
        assert_eq!(
            policy
                .get("checkout_suppressed")
                .and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            selector_table(&policy)
                .get("kind")
                .and_then(toml::Value::as_str),
            Some(expected_selector)
        );
    }
}

#[test]
fn worktree_add_no_checkout_persists_selective_hydration_selectors() {
    let cases: &[(&str, &[&str], &str, &[(&str, &str)])] = &[
        (
            "patterns",
            &["--hydrate-include", "*.bin", "--hydrate-exclude", "skip/**"],
            "patterns",
            &[("include", "*.bin"), ("exclude", "skip/**")],
        ),
        (
            "manifest",
            &["--hydrate-manifest", "manifest.txt"],
            "manifest",
            &[("path", "manifest.txt")],
        ),
        (
            "manifest-ref",
            &["--hydrate-manifest-ref", "refs/heads/main:manifest.txt"],
            "manifest-ref",
            &[("spec", "refs/heads/main:manifest.txt")],
        ),
        (
            "profile",
            &["--hydrate-profile", "hot"],
            "profile",
            &[("name", "hot")],
        ),
    ];

    for (name, selector_args, expected_kind, expected_fields) in cases {
        let Some(tmp) = repo_fixture() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        let repo = tmp.path().join("repo");
        let requested = tmp.path().join(format!("selective-{name}"));
        let mut args = vec!["worktree", "add", "--quiet", "--detach", "--no-checkout"];
        args.extend_from_slice(selector_args);
        args.push(requested.to_str().unwrap());
        args.push("HEAD");

        let output = Command::new(crab_bin())
            .args(args)
            .current_dir(&repo)
            .output()
            .expect("crab output");

        assert!(
            output.status.success(),
            "case {name} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let policy = hydration_policy_for(&requested);
        assert_eq!(toml_string(&policy, "source"), Some("explicit"));
        assert_eq!(toml_string(&policy, "status"), Some("pending"));
        assert_eq!(toml_string(&policy, "mode"), Some("selective"));
        let selector = selector_table(&policy);
        assert_eq!(
            selector.get("kind").and_then(toml::Value::as_str),
            Some(*expected_kind)
        );
        for (field, expected) in *expected_fields {
            if let Some(array) = selector.get(*field).and_then(toml::Value::as_array) {
                assert_eq!(array[0].as_str(), Some(*expected));
            } else {
                assert_eq!(
                    selector.get(*field).and_then(toml::Value::as_str),
                    Some(*expected)
                );
            }
        }
    }
}

#[test]
fn worktree_add_checked_out_runs_selective_hydration_policies() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    std::fs::write(repo.join("manifest.txt"), b"*.bin\n").expect("manifest");
    run_git(&repo, ["add", "manifest.txt"]).expect("git add manifest");
    let commit = run_git(&repo, ["commit", "-qm", "add hydrate manifest"]).expect("commit");
    if !commit.status.success() {
        eprintln!("SKIP: failed to commit hydrate fixture");
        return;
    }

    let cases: &[(&str, &[&str], &str)] = &[
        (
            "patterns",
            &["--hydrate-include", "*.bin", "--hydrate-exclude", "skip/**"],
            "patterns",
        ),
        (
            "manifest",
            &["--hydrate-manifest", "manifest.txt"],
            "manifest",
        ),
        (
            "manifest-ref",
            &["--hydrate-manifest-ref", "HEAD:manifest.txt"],
            "manifest-ref",
        ),
    ];

    for (name, selector_args, expected_kind) in cases {
        let requested = tmp.path().join(format!("selective-checked-out-{name}"));
        let mut args = vec!["worktree", "add", "--quiet", "--detach"];
        args.extend_from_slice(selector_args);
        args.push(requested.to_str().unwrap());
        args.push("HEAD");

        let output = Command::new(crab_bin())
            .args(args)
            .current_dir(&repo)
            .output()
            .expect("crab output");

        assert!(
            output.status.success(),
            "case {name} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("No pointer files match"),
            "case {name} stdout: {stdout}"
        );
        let policy = hydration_policy_for(&requested);
        assert_eq!(toml_string(&policy, "source"), Some("explicit"));
        assert_eq!(toml_string(&policy, "status"), Some("applied"));
        assert_eq!(toml_string(&policy, "mode"), Some("selective"));
        assert_eq!(
            policy
                .get("checkout_suppressed")
                .and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            selector_table(&policy)
                .get("kind")
                .and_then(toml::Value::as_str),
            Some(*expected_kind)
        );
    }
}

#[test]
fn worktree_add_accepts_bounded_no_checkout_prefetch_without_materializing_files() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let requested = tmp.path().join("prefetch-patterns");

    let output = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            "--no-checkout",
            "--prefetch",
            "--hydrate-include",
            "*.bin",
            requested.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requested.exists());
    assert!(!requested.join("a.txt").exists());
    let policy = hydration_policy_for(&requested);
    assert_eq!(toml_string(&policy, "source"), Some("explicit"));
    assert_eq!(toml_string(&policy, "status"), Some("pending"));
    assert_eq!(toml_string(&policy, "mode"), Some("lazy"));
    assert_eq!(
        policy.get("prefetch").and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        policy
            .get("checkout_suppressed")
            .and_then(toml::Value::as_bool),
        Some(true)
    );
    let selector = selector_table(&policy);
    assert_eq!(
        selector.get("kind").and_then(toml::Value::as_str),
        Some("patterns")
    );
}

#[test]
fn worktree_add_prefetch_uses_eager_clone_defaults_as_cache_only_selector() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    std::fs::write(
        repo.join("crab.toml"),
        b"[remote]\nurl = \"crab://bucket/repo\"\n\n[hydrate]\ndefault = \"eager\"\n",
    )
    .expect("project config");
    let requested = tmp.path().join("prefetch-clone-defaults");

    let output = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            "--prefetch",
            requested.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requested.join("a.txt").exists());
    let policy = hydration_policy_for(&requested);
    assert_eq!(toml_string(&policy, "source"), Some("clone-defaults"));
    assert_eq!(toml_string(&policy, "status"), Some("applied"));
    assert_eq!(toml_string(&policy, "mode"), Some("lazy"));
    assert_eq!(
        policy.get("prefetch").and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        selector_table(&policy)
            .get("kind")
            .and_then(toml::Value::as_str),
        Some("all")
    );
}

#[test]
fn worktree_add_lazy_with_prefetch_stays_cache_only() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let requested = tmp.path().join("lazy-prefetch");

    let output = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            "--hydrate=lazy",
            "--prefetch",
            "--hydrate-include",
            "*.bin",
            requested.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requested.join("a.txt").exists());
    let policy = hydration_policy_for(&requested);
    assert_eq!(toml_string(&policy, "source"), Some("explicit"));
    assert_eq!(toml_string(&policy, "status"), Some("applied"));
    assert_eq!(toml_string(&policy, "mode"), Some("lazy"));
    assert_eq!(
        policy.get("prefetch").and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        policy
            .get("checkout_suppressed")
            .and_then(toml::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        selector_table(&policy)
            .get("kind")
            .and_then(toml::Value::as_str),
        Some("patterns")
    );
}

#[test]
fn worktree_add_lazy_and_pointer_only_disable_filters_and_persist_policy() {
    for policy_name in ["lazy", "pointer-only"] {
        let Some(tmp) = repo_fixture() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        let repo = tmp.path().join("repo");
        run_git(&repo, ["config", "filter.crab.clean", "cat"]).expect("clean config");
        run_git(&repo, ["config", "filter.crab.smudge", "false"]).expect("smudge config");
        run_git(&repo, ["config", "filter.crab.required", "true"]).expect("required config");
        std::fs::write(repo.join(".gitattributes"), b"*.bin filter=crab\n").expect("attributes");
        std::fs::write(repo.join("model.bin"), b"pointer bytes\n").expect("model");
        run_git(&repo, ["add", ".gitattributes", "model.bin"]).expect("git add");
        let commit = run_git(&repo, ["commit", "-qm", "add filtered model"]).expect("commit");
        if !commit.status.success() {
            eprintln!("SKIP: failed to commit filtered model");
            return;
        }

        let requested = tmp.path().join(format!("{policy_name}-worktree"));
        let output = Command::new(crab_bin())
            .args([
                "worktree",
                "add",
                "--quiet",
                "--detach",
                &format!("--hydrate={policy_name}"),
                requested.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo)
            .output()
            .expect("crab output");

        assert!(
            output.status.success(),
            "policy {policy_name} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(requested.join("model.bin")).expect("model in worktree"),
            b"pointer bytes\n"
        );
        let policy = hydration_policy_for(&requested);
        assert_eq!(toml_string(&policy, "source"), Some("explicit"));
        assert_eq!(toml_string(&policy, "status"), Some("applied"));
        assert_eq!(toml_string(&policy, "mode"), Some(policy_name));
    }
}

#[test]
fn worktree_add_hydrate_full_runs_post_create_hydration_and_marks_policy_applied() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let requested = tmp.path().join("full-hydrate-no-pointers");

    let output = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            "--hydrate=full",
            requested.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requested.join(".git").exists());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No pointer files match"),
        "stdout: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("hydrating selected Crab pointer files"),
        "stderr: {stderr}"
    );
    let policy = hydration_policy_for(&requested);
    assert_eq!(toml_string(&policy, "status"), Some("applied"));
    assert_eq!(toml_string(&policy, "mode"), Some("full"));
    assert_eq!(
        policy
            .get("checkout_suppressed")
            .and_then(toml::Value::as_bool),
        Some(false)
    );
}

#[test]
fn worktree_add_hydration_failure_preserves_worktree_and_pending_policy() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    std::fs::write(
        repo.join(".gitattributes"),
        b"*.bin filter=crab diff=crab merge=crab -text\n",
    )
    .expect("attributes");
    let pointer = crab_types::pointer::Pointer {
        file_hash: [7; 32],
        size: 4096,
        shard_hint: None,
    };
    std::fs::write(repo.join("missing.bin"), pointer.serialize()).expect("pointer file");
    run_git(&repo, ["add", ".gitattributes", "missing.bin"]).expect("git add pointer");
    let commit = run_git(&repo, ["commit", "-qm", "add missing pointer"]).expect("commit");
    if !commit.status.success() {
        eprintln!("SKIP: failed to commit pointer fixture");
        return;
    }

    let requested = tmp.path().join("failed-hydrate");
    let output = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            "--hydrate=full",
            requested.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(!output.status.success());
    assert!(requested.join(".git").exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("post-create hydration failed"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("crab hydrate"), "stderr: {stderr}");
    let policy = hydration_policy_for(&requested);
    assert_eq!(toml_string(&policy, "status"), Some("pending"));
    assert_eq!(toml_string(&policy, "mode"), Some("full"));
}

#[test]
fn worktree_add_rejects_unbounded_prefetch_before_mutation() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let requested = tmp.path().join("unbounded-prefetch");

    let output = Command::new(crab_bin())
        .args([
            "worktree",
            "add",
            "--detach",
            "--prefetch",
            requested.to_str().unwrap(),
            "HEAD",
        ])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(!output.status.success());
    assert!(!requested.exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--prefetch requires a bounded selection"),
        "stderr: {stderr}"
    );
}

#[test]
fn worktree_add_rejects_version_gated_option_before_mutation() {
    let unsupported = ["--orphan", "--relative-paths", "--no-relative-paths"]
        .into_iter()
        .filter(|option| !git_worktree_add_help_contains(option))
        .collect::<Vec<_>>();
    if unsupported.is_empty() {
        eprintln!("SKIP: installed Git supports tracked version-gated add options");
        return;
    }
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");

    for option in unsupported {
        let requested = tmp
            .path()
            .join(format!("{}-location", option.trim_start_matches("--")));
        let output = Command::new(crab_bin())
            .args(["worktree", "add", option, requested.to_str().unwrap()])
            .current_dir(&repo)
            .output()
            .expect("crab output");

        assert!(!output.status.success());
        assert!(!requested.exists());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(option), "stderr: {stderr}");
        assert!(stderr.contains("crab worktree add"), "stderr: {stderr}");
        assert!(stderr.contains("not supported"), "stderr: {stderr}");
    }
}

#[test]
fn worktree_list_porcelain_matches_git() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");

    let git = run_git(&repo, ["worktree", "list", "--porcelain"]).expect("git output");
    let crab = Command::new(crab_bin())
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert_eq!(git.status.success(), crab.status.success());
    assert_eq!(git.stdout, crab.stdout);
}

#[test]
fn worktree_list_porcelain_z_matches_git() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");

    let git = run_git(&repo, ["worktree", "list", "--porcelain", "-z"]).expect("git output");
    let crab = Command::new(crab_bin())
        .args(["worktree", "list", "--porcelain", "-z"])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert_eq!(git.status.success(), crab.status.success());
    assert_eq!(git.stdout, crab.stdout);
}

#[test]
fn worktree_list_json_reports_git_records_and_crab_identities() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");

    let output = Command::new(crab_bin())
        .args(["worktree", "list", "--json"])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json envelope");
    assert_eq!(value["schema"], "worktree.list");

    let worktrees = value["data"]["worktrees"]
        .as_array()
        .expect("worktrees array");
    assert_eq!(worktrees.len(), 2);
    assert_eq!(worktrees[0]["crab"]["identity"], "main");
    assert_eq!(worktrees[1]["crab"]["identity"], "linked");
    assert!(worktrees[0]["path"].as_str().is_some());
    assert!(worktrees[1]["detached"].as_bool().unwrap_or(false));
}

#[test]
fn worktree_mutating_json_reports_payloads_and_changes_git_state() {
    let Some(tmp) = repo_fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("json-linked");
    let linked_str = linked.to_str().expect("utf8 linked path");

    let add = Command::new(crab_bin())
        .args(["worktree", "--json", "add", "--detach", linked_str, "HEAD"])
        .current_dir(&repo)
        .output()
        .expect("crab add");
    let add_json = json_envelope(&add);
    let linked_canonical = linked.canonicalize().expect("canonical linked path");
    let linked_canonical_str = linked_canonical.to_str().expect("utf8 canonical path");
    assert_eq!(add_json["schema"], "worktree.add");
    assert_eq!(add_json["data"]["created"], true);
    assert_eq!(add_json["data"]["worktree"]["path"], linked_canonical_str);
    assert!(worktree_record_for_path(&repo, &linked).is_some());

    let lock = Command::new(crab_bin())
        .args(["worktree", "--json", "lock", "--reason", "json", linked_str])
        .current_dir(&repo)
        .output()
        .expect("crab lock");
    let lock_json = json_envelope(&lock);
    assert_eq!(lock_json["schema"], "worktree.lock");
    assert_eq!(lock_json["data"]["path"], linked_canonical_str);
    assert!(
        worktree_record_for_path(&repo, &linked)
            .expect("locked record")
            .locked
    );

    let repair = Command::new(crab_bin())
        .args(["worktree", "--json", "repair", linked_str])
        .current_dir(&repo)
        .output()
        .expect("crab repair");
    let repair_json = json_envelope(&repair);
    assert_eq!(repair_json["schema"], "worktree.repair");
    assert_eq!(repair_json["data"]["repaired"], true);
    assert_eq!(repair_json["data"]["paths"][0], linked_str);

    let unlock = Command::new(crab_bin())
        .args(["worktree", "--json", "unlock", linked_str])
        .current_dir(&repo)
        .output()
        .expect("crab unlock");
    let unlock_json = json_envelope(&unlock);
    assert_eq!(unlock_json["schema"], "worktree.unlock");
    assert_eq!(unlock_json["data"]["path"], linked_canonical_str);

    let remove = Command::new(crab_bin())
        .args(["worktree", "--json", "remove", "--force", linked_str])
        .current_dir(&repo)
        .output()
        .expect("crab remove");
    let remove_json = json_envelope(&remove);
    assert_eq!(remove_json["schema"], "worktree.remove");
    assert_eq!(remove_json["data"]["path"], linked_canonical_str);
    assert!(worktree_record_for_path(&repo, &linked).is_none());
}

#[test]
#[cfg(unix)]
fn worktree_list_json_with_crab_state_reports_state_summaries() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(&linked).expect("linked ctx");
    let content = b"large";
    let hydrated_path = linked.join("large.bin");
    std::fs::write(&hydrated_path, content).expect("write hydrated content");
    let pointer = crab_types::pointer::Pointer {
        file_hash: *blake3::hash(content).as_bytes(),
        size: content.len() as u64,
        shard_hint: None,
    }
    .serialize();
    let entry = crab::cache::hydrated_pointer::entry_for_path(&hydrated_path, &pointer)
        .expect("cacheable hydrated content");
    crab::cache::HydratedPointerCache::update_on_disk(
        &ctx.per_worktree_crab_dir
            .join(crab::cache::hydrated_pointer::HYDRATED_POINTERS_FILENAME),
        [("large.bin".to_owned(), entry)],
    )
    .expect("save hydrated cache");
    std::fs::write(
        ctx.per_worktree_crab_dir.join("hydration-policy.toml"),
        b"mode = \"full\"\n",
    )
    .expect("policy");
    std::fs::write(ctx.per_worktree_crab_dir.join("access.db"), b"sqlite").expect("access db");

    let output = Command::new(crab_bin())
        .args(["worktree", "list", "--json", "--with-crab-state"])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json envelope");
    let worktrees = value["data"]["worktrees"]
        .as_array()
        .expect("worktrees array");
    let linked_entry = worktrees
        .iter()
        .find(|entry| entry["crab"]["identity"] == "linked")
        .expect("linked entry");
    let state = &linked_entry["crab"]["state"];
    assert_eq!(state["hydrated_pointer_cache"]["entries"], 1);
    assert_eq!(state["pointer_summary"]["hydrated_pointer_entries"], 1);
    assert_eq!(state["hydration_policy"]["exists"], true);
    assert_eq!(state["access_db"]["exists"], true);
}

#[test]
fn worktree_list_default_json_omits_crab_state_summaries() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(&linked).expect("linked ctx");
    std::fs::create_dir_all(&ctx.per_worktree_crab_dir).expect("state dir");
    std::fs::write(
        ctx.per_worktree_crab_dir.join("hydrated-pointers.json"),
        b"not-json",
    )
    .expect("corrupt cache");

    let output = Command::new(crab_bin())
        .args(["worktree", "list", "--json"])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json envelope");
    let worktrees = value["data"]["worktrees"]
        .as_array()
        .expect("worktrees array");
    assert!(
        worktrees
            .iter()
            .all(|entry| entry["crab"]["state"].is_null())
    );
}

#[cfg(unix)]
#[test]
fn worktree_list_default_json_does_not_read_state_or_worktree_contents() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(&linked).expect("linked ctx");
    let unreadable_worktree_dir = linked.join("private");
    std::fs::create_dir(&unreadable_worktree_dir).expect("private dir");
    std::fs::write(unreadable_worktree_dir.join("hidden.txt"), b"hidden").expect("hidden file");
    std::fs::create_dir_all(&ctx.per_worktree_crab_dir).expect("state dir");
    std::fs::write(
        ctx.per_worktree_crab_dir.join("hydrated-pointers.json"),
        b"not-json",
    )
    .expect("corrupt cache");
    std::fs::set_permissions(
        &unreadable_worktree_dir,
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("chmod worktree dir");
    std::fs::set_permissions(
        &ctx.per_worktree_crab_dir,
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("chmod state dir");

    let output = Command::new(crab_bin())
        .args(["worktree", "list", "--json"])
        .current_dir(&repo)
        .output()
        .expect("crab output");

    std::fs::set_permissions(
        &ctx.per_worktree_crab_dir,
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("restore state dir");
    std::fs::set_permissions(
        &unreadable_worktree_dir,
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("restore worktree dir");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json envelope");
    let worktrees = value["data"]["worktrees"]
        .as_array()
        .expect("worktrees array");
    assert!(
        worktrees
            .iter()
            .all(|entry| entry["crab"]["state"].is_null())
    );
}

#[test]
fn hydrate_clear_speculation_clears_current_worktree_access_db_only() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo");
    let linked = tmp.path().join("linked");
    let main_ctx = crab::git::worktree::WorktreeContext::resolve_from(&repo).expect("main ctx");
    let linked_ctx =
        crab::git::worktree::WorktreeContext::resolve_from(&linked).expect("linked ctx");
    let main_db_path = crab::speculation::access_db::path_for_context(&main_ctx);
    let linked_db_path = crab::speculation::access_db::path_for_context(&linked_ctx);
    std::fs::create_dir_all(main_db_path.parent().unwrap()).expect("main db parent");
    std::fs::create_dir_all(linked_db_path.parent().unwrap()).expect("linked db parent");
    {
        let main_db = crab::speculation::access_db::AccessDb::open(&main_db_path).expect("main db");
        main_db
            .upsert_co_access("main-a", "main-b", 1000)
            .expect("main co-access");
        let linked_db =
            crab::speculation::access_db::AccessDb::open(&linked_db_path).expect("linked db");
        linked_db
            .upsert_co_access("linked-a", "linked-b", 1000)
            .expect("linked co-access");
    }

    let output = Command::new(crab_bin())
        .args(["hydrate", "--clear-speculation"])
        .current_dir(&linked)
        .output()
        .expect("crab hydrate clear");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let main_db = crab::speculation::access_db::AccessDb::open(&main_db_path).expect("main db");
    let linked_db =
        crab::speculation::access_db::AccessDb::open(&linked_db_path).expect("linked db");
    assert_eq!(
        main_db.top_k("main-a", 1, 1).expect("main top k"),
        vec!["main-b".to_owned()]
    );
    assert!(
        linked_db
            .top_k("linked-a", 1, 1)
            .expect("linked top k")
            .is_empty()
    );
}

#[test]
fn worktree_porcelain_parser_handles_line_and_nul_records() {
    let text = b"worktree /repo\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\nworktree /linked\nHEAD 2222222222222222222222222222222222222222\ndetached\n\n";
    let parsed =
        crab::git::worktree::parse_worktree_list_porcelain(text, false).expect("parse text");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].branch.as_deref(), Some("refs/heads/main"));
    assert!(parsed[1].detached);

    let nul = b"worktree /repo\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0worktree /linked\0HEAD 2222222222222222222222222222222222222222\0detached\0\0";
    let parsed = crab::git::worktree::parse_worktree_list_porcelain(nul, true).expect("parse nul");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[1].path, "/linked");
}

#[test]
fn worktree_help_exposes_list_subcommand() {
    let output = Command::new(crab_bin())
        .args(["worktree", "--help"])
        .output()
        .expect("crab output");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("add"));
    assert!(stdout.contains("list"));
}

#[test]
fn compatibility_matrix_records_local_version_and_newer_options() {
    let version = crab::git::worktree::installed_git_version()
        .expect("git version probe")
        .map(|version| version.original)
        .unwrap_or_else(|| "git unavailable".to_owned());
    eprintln!("installed git: {version}");

    let surface = crab::git::worktree::GIT_2_39_WORKTREE_SURFACE;
    assert!(surface.iter().any(|entry| entry.subcommand == "add"));
    assert!(surface.iter().any(|entry| entry.subcommand == "list"));
    assert!(surface.iter().any(|entry| entry.subcommand == "repair"));

    let latest = crab::git::worktree::LATEST_TRACKED_VERSION_GATED_OPTIONS;
    assert!(latest.iter().any(|entry| entry.option == "--orphan"));
    assert!(
        latest
            .iter()
            .any(|entry| entry.option == "--relative-paths")
    );
    assert!(
        latest
            .iter()
            .any(|entry| entry.option == "--no-relative-paths")
    );

    let help = Command::new("git").args(["worktree", "-h"]).output();
    let Ok(help) = help else {
        eprintln!("SKIP: git worktree help unavailable");
        return;
    };
    let help_text = format!(
        "{}{}",
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    );
    let unsupported: Vec<&str> = latest
        .iter()
        .map(|entry| entry.option)
        .filter(|option| !help_text.contains(option))
        .collect();
    if !unsupported.is_empty() {
        eprintln!("version-gated worktree option tests skipped for: {unsupported:?}");
        return;
    }
}
