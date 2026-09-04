use super::*;
use std::path::PathBuf;
use std::time::Duration;

struct Fixture {
    directory: tempfile::TempDir,
    upstream: PathBuf,
    client: PathBuf,
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(root)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Pull test")
        .env("GIT_AUTHOR_EMAIL", "pull@example.invalid")
        .env("GIT_COMMITTER_NAME", "Pull test")
        .env("GIT_COMMITTER_EMAIL", "pull@example.invalid");
    for key in GIT_ENV_REMOVALS {
        command.env_remove(key);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn commit(root: &Path, path: &str, content: &str) {
    std::fs::write(root.join(path), content).unwrap();
    git(root, &["add", "--", path]);
    git(root, &["commit", "-m", "Update fixture"]);
}

fn fixture() -> Fixture {
    // The path intentionally includes a former diagnostic keyword. It must
    // not turn an unrelated fetch/merge refusal into a conflict result.
    let directory = tempfile::Builder::new()
        .prefix("conflict-pull-")
        .tempdir()
        .unwrap();
    let upstream = directory.path().join("upstream");
    let client = directory.path().join("client");
    std::fs::create_dir(&upstream).unwrap();
    git(&upstream, &["init", "--initial-branch=main"]);
    commit(&upstream, "file.txt", "base\n");
    git(
        directory.path(),
        &[
            "clone",
            upstream.to_str().unwrap(),
            client.to_str().unwrap(),
        ],
    );
    git(&client, &["config", "user.name", "Pull test"]);
    git(&client, &["config", "user.email", "pull@example.invalid"]);
    git(&client, &["config", "pull.rebase", "false"]);
    Fixture {
        directory,
        upstream,
        client,
    }
}

#[test]
fn merge_conflict_comes_from_index_in_both_output_modes() {
    for progress in [false, true] {
        let fixture = fixture();
        commit(&fixture.upstream, "file.txt", "remote\n");
        commit(&fixture.client, "file.txt", "local\n");
        let result = pull_in(
            &fixture.client,
            "origin",
            Some("main"),
            progress,
            &CancellationToken::new(),
        );
        assert!(
            matches!(result, Err(CrabError::PullConflict { files, .. }) if files == ["file.txt"])
        );
    }
}

#[test]
fn unrelated_refusal_is_not_a_conflict_because_of_remote_path() {
    let fixture = fixture();
    commit(&fixture.upstream, "file.txt", "remote\n");
    commit(&fixture.client, "local.txt", "local\n");
    git(&fixture.client, &["config", "pull.ff", "only"]);
    let result = pull_in(
        &fixture.client,
        "origin",
        Some("main"),
        false,
        &CancellationToken::new(),
    );
    assert!(
        matches!(result, Err(CrabError::Io(error)) if error.to_string().contains("fast-forward"))
    );
}

#[test]
fn failed_transport_keeps_diagnostics_in_both_output_modes() {
    for progress in [false, true] {
        let fixture = fixture();
        let missing = fixture.directory.path().join("missing");
        git(
            &fixture.client,
            &["remote", "set-url", "origin", missing.to_str().unwrap()],
        );
        let result = pull_in(
            &fixture.client,
            "origin",
            Some("main"),
            progress,
            &CancellationToken::new(),
        );
        assert!(
            matches!(result, Err(CrabError::PullRemoteUnreachable { reason, .. }) if reason.contains("Could not read from remote repository"))
        );
    }
}

#[test]
fn no_op_merge_and_rebase_ignore_stale_orig_head() {
    for rebase in ["false", "true"] {
        let fixture = fixture();
        let cancel = CancellationToken::new();
        let initial = head(&fixture.client, &cancel).unwrap().unwrap();
        commit(&fixture.upstream, "file.txt", "remote\n");
        pull_in(&fixture.client, "origin", Some("main"), false, &cancel).unwrap();
        git(&fixture.client, &["config", "pull.rebase", rebase]);
        git(&fixture.client, &["update-ref", "ORIG_HEAD", &initial]);
        let changed = pull_in(&fixture.client, "origin", Some("main"), false, &cancel).unwrap();
        assert!(changed.is_empty());
    }
}

#[test]
fn fast_forward_returns_exact_add_modify_delete_and_rename_paths() {
    let fixture = fixture();
    git(&fixture.upstream, &["mv", "file.txt", "renamed.txt"]);
    commit(&fixture.upstream, "new.txt", "new\n");
    let changed = pull_in(
        &fixture.client,
        "origin",
        Some("main"),
        false,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(changed, ["file.txt", "new.txt", "renamed.txt"]);
}

#[test]
fn initial_pull_lists_the_new_tree() {
    let fixture = fixture();
    let empty = fixture.directory.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    git(&empty, &["init", "--initial-branch=main"]);
    git(
        &empty,
        &[
            "remote",
            "add",
            "origin",
            fixture.upstream.to_str().unwrap(),
        ],
    );
    let changed = pull_in(
        &empty,
        "origin",
        Some("main"),
        false,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(changed, ["file.txt"]);
}

#[test]
fn merge_and_rebase_return_only_paths_changed_from_the_local_snapshot() {
    for rebase in ["false", "true"] {
        let fixture = fixture();
        commit(&fixture.client, "local.txt", "local\n");
        commit(&fixture.upstream, "file.txt", "remote\n");
        git(&fixture.client, &["config", "pull.rebase", rebase]);
        let changed = pull_in(
            &fixture.client,
            "origin",
            Some("main"),
            false,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(changed, ["file.txt"], "pull.rebase={rebase}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn successful_pull_does_not_cancel_its_callers_other_work() {
    let fixture = fixture();
    let cancel = CancellationToken::new();
    pull(&fixture.client, "origin", Some("main"), false, &cancel)
        .await
        .unwrap();
    assert!(!cancel.is_cancelled());
}

#[test]
fn corrupt_head_is_not_an_unborn_branch() {
    let fixture = fixture();
    std::fs::write(
        fixture.client.join(".git/refs/heads/main"),
        format!("{}\n", "f".repeat(40)),
    )
    .unwrap();
    assert!(head(&fixture.client, &CancellationToken::new()).is_err());
}

#[test]
fn failed_snapshot_diff_does_not_fall_back_to_orig_head() {
    let fixture = fixture();
    let cancel = CancellationToken::new();
    let after = head(&fixture.client, &cancel).unwrap().unwrap();
    let result = changed_paths(&fixture.client, Some(&"f".repeat(40)), &after, &cancel);
    assert!(result.is_err());
}

#[test]
fn nul_inventory_preserves_literal_whitespace_and_metacharacters() {
    assert_eq!(
        parse_paths(b" leading \0dir/a[b]*?.bin\0new\nline\0").unwrap(),
        [" leading ", "dir/a[b]*?.bin", "new\nline"]
    );
}

#[test]
fn malformed_inventory_cannot_select_other_paths() {
    for bytes in [
        b"unterminated".as_slice(),
        b"\0",
        b"a\0\0",
        b"../a\0",
        b"/a\0",
        b"a/../b\0",
        b"a//b\0",
        b".git/config\0",
        b"bad-\xff\0",
    ] {
        assert!(parse_paths(bytes).is_err(), "{bytes:?}");
    }
}

#[test]
fn progress_is_teed_without_losing_captured_diagnostics() {
    let mut visible = Vec::new();
    let captured = capture_output(
        ProgressReader {
            source: &b"progress\nerror\n"[..],
            sink: &mut visible,
        },
        15,
    )
    .unwrap();
    assert_eq!(captured, visible);
}

#[test]
fn progress_capture_remains_bounded() {
    let mut visible = Vec::new();
    let result = capture_output(
        ProgressReader {
            source: &b"too much"[..],
            sink: &mut visible,
        },
        3,
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
}

#[test]
fn progress_sink_failure_preserves_io_source() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let result = capture_output(
        ProgressReader {
            source: &b"progress"[..],
            sink: Broken,
        },
        20,
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn cancellation_precedes_repository_access() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = pull(
        Path::new("missing-repository"),
        "origin",
        None,
        false,
        &cancel,
    )
    .await;
    assert!(matches!(result, Err(CrabError::Cancelled)));
}

#[test]
fn stalled_hook_fixture() {
    let Some(path) = std::env::var_os("CRAB_PULL_HOOK_READY") else {
        return;
    };
    let path = PathBuf::from(path);
    let mut counter = 0_u64;
    loop {
        std::fs::write(&path, counter.to_string()).unwrap();
        counter += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_joins_git_and_its_hook_before_returning() {
    let fixture = fixture();
    commit(&fixture.upstream, "file.txt", "remote\n");
    let ready = fixture.directory.path().join("hook-ready");
    let quoted = |path: &Path| {
        format!(
            "'{}'",
            path.to_str()
                .unwrap()
                .replace('\\', "/")
                .replace('\'', "'\\''")
        )
    };
    let executable = quoted(&std::env::current_exe().unwrap());
    let hook = fixture.client.join(".git/hooks/post-merge");
    std::fs::write(&hook, format!("#!/bin/sh\nCRAB_PULL_HOOK_READY={} exec {executable} --exact cmd::pull::git::tests::stalled_hook_fixture --nocapture\n", quoted(&ready))).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let marker = ready.clone();
    let watcher = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = marker.exists();
        stop.cancel();
        started
    });
    let result = pull(&fixture.client, "origin", Some("main"), false, &cancel).await;
    assert!(watcher.join().unwrap(), "hook was not reached");
    assert!(matches!(result, Err(CrabError::Cancelled)));
    let before = std::fs::read(&ready).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        std::fs::read(&ready).unwrap(),
        before,
        "hook remained live after cancellation"
    );
}
