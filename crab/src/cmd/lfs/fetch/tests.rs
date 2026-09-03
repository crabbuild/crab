use super::*;

mod all;

#[test]
fn cancelled_fetch_and_pull_stop_before_repository_resolution() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        run_lfs_fetch(LfsFetchOptions::default(), &cancel),
        Err(CrabError::Cancelled)
    ));
    assert!(matches!(
        run_lfs_pull(LfsPullOptions::default(), &cancel),
        Err(CrabError::Cancelled)
    ));
}
use sha2::Digest;
use std::fs;

fn pointer(data: &[u8]) -> LfsPointer {
    let oid: [u8; 32] = sha2::Sha256::digest(data).into();
    LfsPointer {
        oid,
        size: data.len() as u64,
        extensions: Vec::new(),
    }
}

fn fixture_git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    for key in crate::git::process::GIT_ENV_REMOVALS {
        command.env_remove(key);
    }
    let output = command.current_dir(root).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit_fixture(root: &Path) {
    fixture_git(root, &["add", "."]);
    fixture_git(
        root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "pointer fixture",
        ],
    );
}

#[test]
fn unborn_default_fetch_is_empty_but_an_invalid_explicit_ref_fails() {
    let temporary = tempfile::tempdir().unwrap();
    fixture_git(temporary.path(), &["init", "-q"]);
    let cancel = CancellationToken::new();
    assert!(
        collect_lfs_pointers(temporary.path(), false, false, &[], &cancel)
            .unwrap()
            .is_empty()
    );
    assert!(
        collect_lfs_pointers(
            temporary.path(),
            false,
            false,
            &["missing-ref".to_owned()],
            &cancel
        )
        .is_err()
    );
}

#[test]
fn fetch_from_subdirectory_preserves_aliases_until_path_policy() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q"]);
    fs::create_dir(root.join("nested")).unwrap();
    let ptr = pointer(b"shared content");
    fs::write(root.join("a.bin"), ptr.serialize()).unwrap();
    fs::write(root.join("nested/z.bin"), ptr.serialize()).unwrap();
    commit_fixture(root);
    let entries = collect_lfs_pointers(
        &root.join("nested"),
        false,
        false,
        &[],
        &CancellationToken::new(),
    )
    .unwrap();
    let include = PatternFilter::new("nested/**").unwrap();
    let transfers = plan_fetch_transfers(
        &entries,
        Some(&include),
        None,
        &root.join("empty-cache"),
        false,
    )
    .unwrap();
    assert_eq!(
        transfers
            .iter()
            .map(|transfer| transfer.path.as_str())
            .collect::<Vec<_>>(),
        ["nested/z.bin"]
    );
    assert_eq!(
        checkout_paths_for_pull(&entries, None, None),
        ["a.bin", "nested/z.bin"]
    );
}

#[test]
fn all_ref_fetch_retains_distinct_pointer_versions_and_rejects_partial_inventory() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q"]);
    let old = pointer(b"old content");
    fs::write(root.join("asset.bin"), old.serialize()).unwrap();
    commit_fixture(root);
    fixture_git(root, &["branch", "old"]);
    let new = pointer(b"new content");
    fs::write(root.join("asset.bin"), new.serialize()).unwrap();
    commit_fixture(root);
    let cancel = CancellationToken::new();
    let entries = collect_lfs_pointers(root, true, false, &[], &cancel).unwrap();
    let oids = entries
        .into_iter()
        .map(|(_, pointer)| pointer.oid)
        .collect::<HashSet<_>>();
    assert_eq!(oids, HashSet::from([old.oid, new.oid]));
    assert!(
        collect_lfs_pointers(
            root,
            false,
            false,
            &["HEAD".to_owned(), "missing-ref".to_owned()],
            &cancel
        )
        .is_err()
    );
}

#[test]
fn all_fetch_includes_replaced_and_deleted_versions_without_old_tip_refs() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q", "-b", "main"]);
    let previous = pointer(b"previous reachable version");
    fs::write(root.join("asset.bin"), previous.serialize()).unwrap();
    commit_fixture(root);
    let current = pointer(b"replacement later deleted");
    fs::write(root.join("asset.bin"), current.serialize()).unwrap();
    commit_fixture(root);
    fixture_git(root, &["rm", "asset.bin"]);
    commit_fixture(root);
    let cancel = CancellationToken::new();
    for refs in [vec![], vec!["main".to_owned()]] {
        let entries = collect_lfs_pointers(root, true, false, &refs, &cancel).unwrap();
        assert_eq!(
            entries
                .into_iter()
                .map(|(_, pointer)| pointer.oid)
                .collect::<HashSet<_>>(),
            HashSet::from([previous.oid, current.oid]),
            "refs={refs:?}"
        );
    }
}

#[test]
fn cancelled_fetch_discovery_never_opens_a_repository() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        collect_lfs_pointers(Path::new("absent"), false, false, &[], &cancel),
        Err(CrabError::Cancelled)
    ));
}

#[test]
fn fetch_discovery_resolves_promised_git_pointer_blobs() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fixture_git(&source, &["init", "-q"]);
    fixture_git(&source, &["config", "uploadpack.allowFilter", "true"]);
    let ptr = pointer(b"promised LFS content");
    fs::write(source.join("asset.bin"), ptr.serialize()).unwrap();
    commit_fixture(&source);
    let remote = url::Url::from_file_path(&source).unwrap().to_string();
    fixture_git(
        temporary.path(),
        &[
            "clone",
            "--filter=blob:none",
            "--no-checkout",
            &remote,
            "reader",
        ],
    );
    let reader = temporary.path().join("reader");
    let missing = fixture_git(
        &reader,
        &["rev-list", "--objects", "--all", "--missing=print"],
    );
    assert!(missing.lines().any(|line| line.starts_with('?')));

    let head = fixture_git(&reader, &["rev-parse", "HEAD"]);
    let cancel = CancellationToken::new();
    assert!(
        crate::lfs::discovery::collect_pointers_from_range_in(&reader, &[head], &[], &cancel)
            .is_err()
    );
    assert_eq!(
        fixture_git(
            &reader,
            &["rev-list", "--objects", "--all", "--missing=print"]
        ),
        missing
    );

    let entries = collect_lfs_pointers(&reader, false, false, &[], &cancel)
        .expect("fetch must resolve promised Git blobs before LFS transfer");

    assert_eq!(entries, [("asset.bin".to_owned(), ptr)]);
}

#[test]
fn recent_fetch_includes_previous_pointer_versions() {
    let _guard = crate::test::git_repo::GIT_DIR_MUTEX.lock().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q"]);
    fixture_git(root, &["config", "lfs.fetchrecentcommitsdays", "7"]);
    fixture_git(root, &["config", "lfs.fetchrecentrefsdays", "7"]);
    fixture_git(root, &["config", "lfs.fetchrecentremoterefs", "false"]);
    let previous = pointer(b"previous version");
    fs::write(root.join("asset.bin"), previous.serialize()).unwrap();
    commit_fixture(root);
    let current = pointer(b"current version");
    fs::write(root.join("asset.bin"), current.serialize()).unwrap();
    commit_fixture(root);
    let entries = collect_lfs_pointers(root, false, true, &[], &CancellationToken::new()).unwrap();
    assert_eq!(
        entries,
        [
            ("asset.bin".to_owned(), current),
            ("asset.bin".to_owned(), previous)
        ]
    );
}

#[test]
fn recent_fetch_finds_versions_replaced_or_deleted_after_the_cutoff() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    for deleted in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fixture_git(root, &["init", "-q"]);
        for args in [
            ["config", "user.name", "Fixture"],
            ["config", "user.email", "fixture@example.invalid"],
            ["config", "commit.gpgsign", "false"],
            ["config", "lfs.fetchrecentcommitsdays", "3"],
            ["config", "lfs.fetchrecentrefsdays", "0"],
        ] {
            fixture_git(root, &args);
        }
        let commit_at = |date: &str| {
            fixture_git(root, &["add", "."]);
            let output = Command::new("git")
                .current_dir(root)
                .args(["commit", "-qm", "dated pointer"])
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let previous = pointer(b"older than the window");
        fs::write(root.join("asset.bin"), previous.serialize()).unwrap();
        commit_at("1000000000 +0000");
        let current = pointer(b"replacement inside the window");
        if deleted {
            fs::remove_file(root.join("asset.bin")).unwrap();
        } else {
            fs::write(root.join("asset.bin"), current.serialize()).unwrap();
        }
        commit_at("1000864000 +0000");
        let entries =
            collect_lfs_pointers(root, false, true, &[], &CancellationToken::new()).unwrap();
        let mut expected = Vec::new();
        if !deleted {
            expected.push(("asset.bin".to_owned(), current));
        }
        expected.push(("asset.bin".to_owned(), previous));
        assert_eq!(entries, expected, "deleted={deleted}");
    }
}

#[test]
fn validate_fetch_rejects_json_with_prune() {
    let options = LfsFetchOptions {
        json: true,
        prune: true,
        ..LfsFetchOptions::default()
    };

    let err = validate_fetch_options(&options).unwrap_err();

    assert!(err.to_string().contains("--json"));
}

#[test]
fn remote_operand_becomes_ref_when_not_configured_and_alone() {
    let options = LfsFetchOptions {
        remote: Some("feature".to_owned()),
        ..LfsFetchOptions::default()
    };

    let (remote, refs) = remote_and_refs_from_options(&options).unwrap();

    assert_eq!(remote, None);
    assert_eq!(refs, vec!["feature"]);
}

#[test]
fn plan_fetch_transfers_skips_local_without_refetch() {
    let dir = tempfile::tempdir().unwrap();
    let ptr = pointer(b"local");
    let local_path = local_object_path(dir.path(), &ptr.oid);
    fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    fs::write(local_path, b"local").unwrap();
    let entries = vec![("asset.bin".to_owned(), ptr)];

    let transfers = plan_fetch_transfers(&entries, None, None, dir.path(), false).unwrap();

    assert!(transfers.is_empty());
}

#[test]
fn plan_fetch_transfers_includes_local_with_refetch() {
    let dir = tempfile::tempdir().unwrap();
    let ptr = pointer(b"local");
    let local_path = local_object_path(dir.path(), &ptr.oid);
    fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    fs::write(local_path, b"local").unwrap();
    let entries = vec![("asset.bin".to_owned(), ptr)];

    let transfers = plan_fetch_transfers(&entries, None, None, dir.path(), true).unwrap();

    assert_eq!(transfers.len(), 1);
}

#[test]
fn plan_fetch_transfers_deduplicates_by_oid() {
    let dir = tempfile::tempdir().unwrap();
    let ptr = pointer(b"same");
    let entries = vec![
        ("a.bin".to_owned(), ptr.clone()),
        ("b.bin".to_owned(), ptr.clone()),
    ];

    let transfers = plan_fetch_transfers(&entries, None, None, dir.path(), false).unwrap();

    assert_eq!(transfers.len(), 1);
    assert_eq!(transfers[0].path, "a.bin");
}

#[test]
fn checkout_paths_for_pull_applies_include_and_exclude() {
    let entries = vec![
        ("models/a.bin".to_owned(), pointer(b"a")),
        ("models/tmp.bin".to_owned(), pointer(b"tmp")),
        ("docs/readme.bin".to_owned(), pointer(b"readme")),
    ];
    let include = PatternFilter::new("models/**").unwrap();
    let exclude = PatternFilter::new("models/tmp.bin").unwrap();

    let paths = checkout_paths_for_pull(&entries, Some(&include), Some(&exclude));

    assert_eq!(paths, vec!["models/a.bin"]);
}

#[test]
fn fetch_json_href_uses_crab_lfs_scheme_and_fanout() {
    let oid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let href = fetch_json_href("repo/path", oid);

    assert_eq!(
        href,
        "crab-lfs://repo/path/lfs/objects/01/23/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
}
