use super::*;

fn fixture_git(root: &Path, args: &[&str]) -> String {
    let output = git_output(root, args, &CancellationToken::new()).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn cancelled_recent_queries_stop_before_repository_access() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let root = Path::new("missing-recent-query-repository");
    assert!(matches!(
        recent_ref_oids_in(root, 0, &cancel),
        Err(CrabError::Cancelled)
    ));
    for revisions in [vec![], vec!["HEAD".to_owned()]] {
        assert!(matches!(
            recent_commit_oids_in(root, &revisions, 0, &cancel),
            Err(CrabError::Cancelled)
        ));
    }
    assert!(matches!(
        git_config_u64_in(root, "lfs.pruneoffsetdays", 3, &cancel),
        Err(CrabError::Cancelled)
    ));
    assert!(matches!(
        git_config_bool_in(root, "lfs.fetchrecentremoterefs", true, &cancel),
        Err(CrabError::Cancelled)
    ));
}

#[test]
fn typed_config_preserves_defaults_and_rejects_invalid_values() {
    let _guard = crate::test::git_repo::GIT_DIR_MUTEX.lock().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q"]);
    let cancel = CancellationToken::new();
    let key = "crab.recentQueryFixture";
    assert_eq!(git_config_u64_in(root, key, 7, &cancel).unwrap(), 7);
    assert!(git_config_bool_in(root, key, true, &cancel).unwrap());
    for (value, expected) in [("0", 0), ("2k", 2048)] {
        fixture_git(root, &["config", key, value]);
        assert_eq!(git_config_u64_in(root, key, 7, &cancel).unwrap(), expected);
    }
    fixture_git(root, &["config", key, "off"]);
    assert!(!git_config_bool_in(root, key, true, &cancel).unwrap());
    for value in ["-1", "invalid"] {
        fixture_git(root, &["config", key, value]);
        assert!(git_config_u64_in(root, key, 7, &cancel).is_err());
    }
    assert!(git_config_bool_in(root, key, true, &cancel).is_err());
}

#[test]
fn supervised_recent_queries_preserve_ref_and_commit_selection() {
    let _guard = crate::test::git_repo::GIT_DIR_MUTEX.lock().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q"]);
    for args in [
        ["config", "user.name", "Fixture"],
        ["config", "user.email", "fixture@example.invalid"],
        ["config", "commit.gpgsign", "false"],
        ["config", "lfs.fetchrecentrefsdays", "7"],
        ["config", "lfs.fetchrecentcommitsdays", "7"],
        ["config", "lfs.fetchrecentremoterefs", "false"],
    ] {
        fixture_git(root, &args);
    }
    fixture_git(root, &["commit", "--allow-empty", "-qm", "first"]);
    let first = fixture_git(root, &["rev-parse", "HEAD"]);
    fixture_git(root, &["update-ref", "refs/remotes/origin/other", &first]);
    fixture_git(root, &["commit", "--allow-empty", "-qm", "second"]);
    let second = fixture_git(root, &["rev-parse", "HEAD"]);
    let cancel = CancellationToken::new();
    assert_eq!(
        recent_ref_oids_in(root, 0, &cancel).unwrap(),
        [second.clone()]
    );
    fixture_git(root, &["config", "lfs.fetchrecentremoterefs", "true"]);
    let mut refs = recent_ref_oids_in(root, 0, &cancel).unwrap();
    refs.sort();
    let mut expected = vec![first.clone(), second.clone()];
    expected.sort();
    assert_eq!(refs, expected);
    assert_eq!(
        recent_commit_oids_in(root, &["HEAD".to_owned()], 0, &cancel).unwrap(),
        [second, first]
    );
    fixture_git(root, &["config", "lfs.fetchrecentrefsdays", "0"]);
    assert!(recent_ref_oids_in(root, 3, &cancel).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn recent_query_output_is_bounded_on_both_pipes() {
    let _guard = crate::test::git_repo::GIT_DIR_MUTEX.lock().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    for redirect in ["", " >&2"] {
        let alias = format!("alias.recent-query=!head -c 67108865 /dev/zero{redirect}");
        let error = git_output(
            temporary.path(),
            &["-c", &alias, "recent-query"],
            &CancellationToken::new(),
        )
        .err()
        .expect("oversized output must fail");
        assert!(
            matches!(error, CrabError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData)
        );
    }
}

fn commit_at(root: &Path, timestamp: u64, message: &str) -> String {
    let date = format!("{timestamp} +0000");
    let output = Command::new("git")
        .args(["commit", "--allow-empty", "-qm", message])
        .current_dir(root)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture_git(root, &["rev-parse", "HEAD"])
}

#[test]
fn recent_commit_windows_are_relative_to_each_selected_tip() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q"]);
    for args in [
        ["config", "user.name", "Fixture"],
        ["config", "user.email", "fixture@example.invalid"],
        ["config", "commit.gpgsign", "false"],
        ["config", "lfs.fetchrecentcommitsdays", "3"],
    ] {
        fixture_git(root, &args);
    }
    let start = 1_000_000_000;
    let day = 24 * 60 * 60;
    let base = commit_at(root, start, "base");
    let older_parent = commit_at(root, start + 2 * day, "older parent");
    let older_tip = commit_at(root, start + 4 * day, "older tip");
    fixture_git(root, &["checkout", "-qb", "newer", &base]);
    let newer_parent = commit_at(root, start + 20 * day, "newer parent");
    let newer_tip = commit_at(root, start + 23 * day, "newer tip");
    let mut selected = recent_commit_oids_in(
        root,
        &[older_tip.clone(), newer_tip.clone()],
        0,
        &CancellationToken::new(),
    )
    .unwrap();
    let mut expected = vec![older_tip, older_parent, newer_tip, newer_parent];
    selected.sort();
    expected.sort();
    assert_eq!(selected, expected);
}

#[test]
fn missing_recent_revision_is_an_error_not_an_empty_history() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q"]);
    fixture_git(root, &["config", "lfs.fetchrecentcommitsdays", "3"]);
    for revision in ["HEAD", "refs/heads/missing", "--all"] {
        assert!(
            recent_commit_oids_in(root, &[revision.to_owned()], 0, &CancellationToken::new())
                .is_err(),
            "invalid selected revision {revision} must fail closed"
        );
    }
}

#[test]
fn prune_offset_extends_only_enabled_recent_commit_windows() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q"]);
    for args in [
        ["config", "user.name", "Fixture"],
        ["config", "user.email", "fixture@example.invalid"],
        ["config", "commit.gpgsign", "false"],
        ["config", "lfs.fetchrecentcommitsdays", "3"],
    ] {
        fixture_git(root, &args);
    }
    let start = 1_000_000_000;
    let day = 24 * 60 * 60;
    let base = commit_at(root, start, "base");
    let parent = commit_at(root, start + 2 * day, "parent");
    let tip = commit_at(root, start + 6 * day, "tip");
    let roots = [tip.clone()];
    let cancel = CancellationToken::new();
    for (offset, expected) in [
        (0, vec![tip.clone()]),
        (2, vec![tip.clone(), parent.clone()]),
        (3, vec![tip.clone(), parent.clone(), base.clone()]),
        (u64::MAX, vec![tip, parent, base]),
    ] {
        assert_eq!(
            recent_commit_oids_in(root, &roots, offset, &cancel).unwrap(),
            expected
        );
    }
    fixture_git(root, &["config", "lfs.fetchrecentcommitsdays", "0"]);
    assert!(
        recent_commit_oids_in(root, &roots, 3, &cancel)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn recent_walk_deduplicates_roots_without_pruning_skewed_ancestors() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q"]);
    for args in [
        ["config", "user.name", "Fixture"],
        ["config", "user.email", "fixture@example.invalid"],
        ["config", "commit.gpgsign", "false"],
        ["config", "lfs.fetchrecentcommitsdays", "3"],
    ] {
        fixture_git(root, &args);
    }
    let start = 1_000_000_000;
    let day = 24 * 60 * 60;
    let future_ancestor = commit_at(root, start + 20 * day, "future ancestor");
    commit_at(root, start, "older parent");
    let tip = commit_at(root, start + 4 * day, "tip");
    fixture_git(root, &["tag", "-a", "-m", "tip", "snapshot"]);
    let selected = recent_commit_oids_in(
        root,
        &[tip.clone(), "HEAD".to_owned(), "snapshot".to_owned()],
        0,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(selected, vec![tip, future_ancestor]);
}

#[cfg(unix)]
#[test]
fn stalled_recent_query_is_cancelled_and_joined() {
    use std::time::{Duration, Instant};
    let _guard = crate::test::git_repo::GIT_DIR_MUTEX.lock().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let ready = root.join("ready");
    let cancel = CancellationToken::new();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            git_output(
                root,
                &[
                    "-c",
                    "alias.recent-query=!echo $$ > ready; exec sleep 30",
                    "recent-query",
                ],
                &cancel,
            )
        });
        let started = Instant::now();
        let mut pid = None;
        while started.elapsed() < Duration::from_secs(15) {
            pid = std::fs::read_to_string(&ready)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            if pid.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cancel.cancel();
        assert!(matches!(worker.join().unwrap(), Err(CrabError::Cancelled)));
        let pid = pid.expect("query must have started").to_string();
        let status = Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "cancelled Git descendant is still alive");
        assert!(started.elapsed() < Duration::from_secs(25));
    });
}

#[test]
fn parse_recent_ref_oids_filters_by_cutoff() {
    let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 100
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 90
cccccccccccccccccccccccccccccccccccccccc bad
";

    let refs = parse_recent_ref_oids(output, 100);

    assert_eq!(refs, vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]);
}

#[test]
fn timestamped_commit_requires_a_valid_timestamp_and_full_object_id() {
    for length in [40, 64] {
        let oid = "a".repeat(length);
        assert_eq!(
            parse_timestamped_commit(&format!("100 {oid}")).unwrap(),
            (100, oid.as_str(), "")
        );
    }
    for malformed in [
        "100",
        "bad aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "100 missing",
        "100 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa trailing",
    ] {
        assert!(parse_timestamped_commit(malformed).is_err());
    }
}

#[test]
fn shared_ancestor_receives_the_widest_reachable_tip_window() {
    let high = "a".repeat(40);
    let low = "b".repeat(40);
    let parent = "c".repeat(40);
    let roots = HashSet::from([high.clone(), low.clone()]);
    for heads in [
        format!("200 {high} {parent}\n100 {low} {parent}\n"),
        format!("100 {low} {parent}\n200 {high} {parent}\n"),
    ] {
        let history = format!("{heads}95 {parent}\n");
        let mut remaining = MAX_CAPTURE_BYTES;
        let mut selected = select_recent_commits(
            history.as_bytes(),
            &roots,
            10,
            &mut remaining,
            &CancellationToken::new(),
        )
        .unwrap();
        selected.sort();
        assert_eq!(selected, vec![high.clone(), low.clone(), parent.clone()]);
    }
}

#[test]
fn streamed_recent_history_reuses_frontier_budget() {
    let tip = format!("{:040x}", 512);
    let roots = HashSet::from([tip.clone()]);
    let mut history = String::new();
    for index in (1..=512).rev() {
        let timestamp = if index == 512 { 100 } else { 1 };
        let parent = if index > 1 {
            format!(" {:040x}", index - 1)
        } else {
            String::new()
        };
        history.push_str(&format!("{timestamp} {index:040x}{parent}\n"));
    }
    let mut remaining = 512;
    let selected = select_recent_commits(
        history.as_bytes(),
        &roots,
        10,
        &mut remaining,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(selected, vec![tip]);
    assert_eq!(
        remaining,
        512 - (40 + std::mem::size_of::<String>() * 2) as u64
    );
}

#[test]
fn recent_history_rejects_incomplete_graphs_and_exhausted_budget() {
    let tip = "a".repeat(40);
    let parent = "b".repeat(40);
    let roots = HashSet::from([tip.clone()]);
    for history in [
        String::new(),
        format!("100 {tip}"),
        format!("100 {tip} {parent}\n"),
        format!("90 {parent}\n100 {tip} {parent}\n"),
        format!("100 {tip}\n100 {tip}\n"),
    ] {
        let mut remaining = MAX_CAPTURE_BYTES;
        assert!(
            select_recent_commits(
                history.as_bytes(),
                &roots,
                10,
                &mut remaining,
                &CancellationToken::new()
            )
            .is_err()
        );
    }
    let history = format!("100 {tip}\n");
    assert!(
        select_recent_commits(
            history.as_bytes(),
            &roots,
            10,
            &mut 1,
            &CancellationToken::new()
        )
        .is_err()
    );
}

#[test]
fn rev_list_empty_errors_are_non_fatal() {
    assert!(rev_list_can_be_empty("fatal: bad default revision 'HEAD'"));
    assert!(rev_list_can_be_empty(
        "fatal: your current branch does not have any commits"
    ));
    assert!(rev_list_can_be_empty(
        "fatal: ambiguous argument 'refs/stash'"
    ));
    assert!(!rev_list_can_be_empty("fatal: object database is corrupt"));
}
