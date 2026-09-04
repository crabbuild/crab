use super::*;

#[test]
fn all_fetch_selects_tag_history_and_detached_head_without_widening_explicit_refs() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q", "-b", "main"]);
    let main = pointer(b"main history");
    fs::write(root.join("main.bin"), main.serialize()).unwrap();
    commit_fixture(root);
    fixture_git(root, &["checkout", "-qb", "side"]);
    let side = pointer(b"tag-only history");
    fs::write(root.join("side.bin"), side.serialize()).unwrap();
    commit_fixture(root);
    fixture_git(root, &["tag", "side-release"]);
    fixture_git(root, &["checkout", "-q", "main"]);
    fixture_git(root, &["branch", "-D", "side"]);
    fixture_git(root, &["checkout", "--detach", "-q"]);
    let detached = pointer(b"detached head");
    fs::write(root.join("detached.bin"), detached.serialize()).unwrap();
    commit_fixture(root);
    let cancel = CancellationToken::new();
    for (refs, expected) in [
        (vec![], HashSet::from([main.oid, side.oid, detached.oid])),
        (vec!["main".to_owned()], HashSet::from([main.oid])),
        (
            vec!["side-release".to_owned()],
            HashSet::from([main.oid, side.oid]),
        ),
    ] {
        let entries = collect_lfs_pointers(root, true, false, &refs, &cancel).unwrap();
        assert_eq!(
            entries
                .into_iter()
                .map(|(_, p)| p.oid)
                .collect::<HashSet<_>>(),
            expected
        );
    }
}

#[test]
fn all_fetch_is_empty_for_unborn_repositories_and_rejects_invalid_operands() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fixture_git(root, &["init", "-q", "-b", "main"]);
    let cancel = CancellationToken::new();
    assert!(
        collect_lfs_pointers(root, true, false, &[], &cancel)
            .unwrap()
            .is_empty()
    );
    fs::write(root.join("asset.bin"), pointer(b"current").serialize()).unwrap();
    commit_fixture(root);
    for invalid in ["missing", "--all", "^HEAD", "HEAD..HEAD", "HEAD\n--all"] {
        assert!(
            collect_lfs_pointers(
                root,
                true,
                false,
                &["HEAD".to_owned(), invalid.to_owned()],
                &cancel
            )
            .is_err(),
            "{invalid:?}"
        );
    }
    cancel.cancel();
    assert!(matches!(
        collect_lfs_pointers(Path::new("absent"), true, false, &[], &cancel),
        Err(CrabError::Cancelled)
    ));
}

#[test]
fn all_fetch_resolves_historical_promisor_blobs_without_enabling_publication_reads() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fixture_git(&source, &["init", "-q", "-b", "main"]);
    fixture_git(&source, &["config", "uploadpack.allowFilter", "true"]);
    let previous = pointer(b"promised historical payload");
    fs::write(source.join("asset.bin"), previous.serialize()).unwrap();
    commit_fixture(&source);
    let current = pointer(b"promised current payload");
    fs::write(source.join("asset.bin"), current.serialize()).unwrap();
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
    let missing_args = ["rev-list", "--objects", "--all", "--missing=print"];
    let missing = fixture_git(&reader, &missing_args);
    assert_eq!(
        missing.lines().filter(|line| line.starts_with('?')).count(),
        2
    );
    let head = fixture_git(&reader, &["rev-parse", "HEAD"]);
    let cancel = CancellationToken::new();
    assert!(
        crate::lfs::discovery::collect_pointers_from_range_in(&reader, &[head], &[], &cancel)
            .is_err()
    );
    assert_eq!(fixture_git(&reader, &missing_args), missing);
    let entries =
        collect_lfs_pointers(&reader, true, false, &["main".to_owned()], &cancel).unwrap();
    assert_eq!(
        entries
            .into_iter()
            .map(|(_, p)| p.oid)
            .collect::<HashSet<_>>(),
        HashSet::from([previous.oid, current.oid])
    );
    assert!(
        !fixture_git(&reader, &missing_args)
            .lines()
            .any(|line| line.starts_with('?'))
    );
}

#[test]
fn all_fetch_rejects_corrupt_historical_blobs_and_conflicting_pointer_sizes() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    for format in ["sha1", "sha256"] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fixture_git(
            root,
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--object-format={format}"),
            ],
        );
        let previous = pointer(b"same payload");
        fs::write(root.join("asset.bin"), previous.serialize()).unwrap();
        commit_fixture(root);
        let old_blob = fixture_git(root, &["rev-parse", "HEAD:asset.bin"]);
        let mut conflicting = previous;
        conflicting.size += 1;
        fs::write(root.join("asset.bin"), conflicting.serialize()).unwrap();
        fs::write(root.join("ordinary.bin"), vec![0x80; 65536]).unwrap();
        commit_fixture(root);
        let cancel = CancellationToken::new();
        let entries = collect_lfs_pointers(root, true, false, &[], &cancel).unwrap();
        assert!(matches!(
            plan_fetch_transfers(&entries, None, None, &root.join("empty-cache"), false),
            Err(CrabError::LfsObjectCorrupt { .. })
        ));
        let large_blob = fixture_git(root, &["rev-parse", "HEAD:ordinary.bin"]);
        let object_path = |oid: &str| root.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
        let replacement = fs::read(object_path(&large_blob)).unwrap();
        fs::remove_file(object_path(&old_blob)).unwrap();
        fs::write(object_path(&old_blob), replacement).unwrap();
        let error = collect_lfs_pointers(root, true, false, &[], &cancel).unwrap_err();
        assert!(
            matches!(error, CrabError::Io(error) if error.kind() == io::ErrorKind::InvalidData && error.to_string().contains("checksum differs"))
        );
    }
}
