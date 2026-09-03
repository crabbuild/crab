use super::*;
use std::collections::HashSet;
use std::fs;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> String {
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

fn pointer(data: &[u8]) -> LfsPointer {
    LfsPointer {
        oid: sha2::Sha256::digest(data).into(),
        size: data.len() as u64,
        extensions: Vec::new(),
    }
}

fn commit(root: &Path) {
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "pointer history",
        ],
    );
}

#[test]
fn bulk_push_includes_replaced_and_deleted_versions_without_old_tip_refs() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    let old = pointer(b"old payload");
    fs::write(root.join("asset.bin"), old.serialize()).unwrap();
    commit(root);
    let new = pointer(b"new payload");
    fs::write(root.join("asset.bin"), new.serialize()).unwrap();
    commit(root);
    git(root, &["rm", "asset.bin"]);
    commit(root);
    for refs in [vec![], vec!["main".to_owned()]] {
        let pointers = collect_push_pointers(root, true, &refs, &CancellationToken::new()).unwrap();
        assert_eq!(
            pointers.into_iter().map(|p| p.oid).collect::<HashSet<_>>(),
            HashSet::from([old.oid, new.oid])
        );
    }
}

#[test]
fn bulk_push_defaults_to_local_branches_and_tags_not_remote_or_detached_history() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    let main = pointer(b"local branch payload");
    fs::write(root.join("main.bin"), main.serialize()).unwrap();
    commit(root);
    let remote = pointer(b"remote-only payload");
    git(root, &["checkout", "-qb", "temporary-remote"]);
    fs::write(root.join("remote.bin"), remote.serialize()).unwrap();
    commit(root);
    git(root, &["update-ref", "refs/remotes/upstream/side", "HEAD"]);
    git(root, &["checkout", "-q", "main"]);
    git(root, &["branch", "-D", "temporary-remote"]);
    let tag = pointer(b"tag-only payload");
    git(root, &["checkout", "--detach", "-q"]);
    fs::write(root.join("tag.bin"), tag.serialize()).unwrap();
    commit(root);
    git(root, &["tag", "release"]);
    git(root, &["checkout", "--detach", "-q", "main"]);
    let detached = pointer(b"detached-only payload");
    fs::write(root.join("detached.bin"), detached.serialize()).unwrap();
    commit(root);
    for (refs, expected) in [
        (vec![], HashSet::from([main.oid, tag.oid])),
        (vec!["main".to_owned()], HashSet::from([main.oid])),
        (
            vec!["refs/remotes/upstream/side".to_owned()],
            HashSet::from([main.oid, remote.oid]),
        ),
        (
            vec!["HEAD".to_owned()],
            HashSet::from([main.oid, detached.oid]),
        ),
    ] {
        let pointers = collect_push_pointers(root, true, &refs, &CancellationToken::new()).unwrap();
        assert_eq!(
            pointers.into_iter().map(|p| p.oid).collect::<HashSet<_>>(),
            expected
        );
    }
}

#[test]
fn bulk_push_never_hydrates_promised_pointer_blobs() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main"]);
    git(&source, &["config", "uploadpack.allowFilter", "true"]);
    for value in [b"old".as_slice(), b"new"] {
        fs::write(source.join("asset.bin"), pointer(value).serialize()).unwrap();
        commit(&source);
    }
    let url = url::Url::from_file_path(&source).unwrap().to_string();
    git(
        dir.path(),
        &[
            "clone",
            "--filter=blob:none",
            "--no-checkout",
            &url,
            "reader",
        ],
    );
    let reader = dir.path().join("reader");
    let missing_args = ["rev-list", "--objects", "--all", "--missing=print"];
    let before = git(&reader, &missing_args);
    assert_eq!(
        before.lines().filter(|line| line.starts_with('?')).count(),
        2
    );
    for refs in [vec![], vec!["main".to_owned()]] {
        assert!(collect_push_pointers(&reader, true, &refs, &CancellationToken::new()).is_err());
        assert_eq!(git(&reader, &missing_args), before);
    }
}

#[test]
fn bulk_push_rejects_invalid_operands_and_honors_precancellation() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    let cancel = CancellationToken::new();
    assert!(
        collect_push_pointers(root, true, &[], &cancel)
            .unwrap()
            .is_empty()
    );
    fs::write(root.join("asset.bin"), pointer(b"payload").serialize()).unwrap();
    commit(root);
    for invalid in ["missing-ref", "--all", "^HEAD", "HEAD..HEAD", "HEAD\n--all"] {
        assert!(
            collect_push_pointers(
                root,
                true,
                &["HEAD".to_owned(), invalid.to_owned()],
                &cancel
            )
            .is_err()
        );
    }
    cancel.cancel();
    for all in [false, true] {
        assert!(matches!(
            collect_push_pointers(Path::new("absent"), all, &[], &cancel),
            Err(CrabError::Cancelled)
        ));
    }
}

#[test]
fn bulk_push_rejects_conflicting_sizes_and_corrupt_historical_blobs() {
    let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
    for format in ["sha1", "sha256"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(
            root,
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--object-format={format}"),
            ],
        );
        let old = pointer(b"same payload");
        fs::write(root.join("asset.bin"), old.serialize()).unwrap();
        commit(root);
        let old_blob = git(root, &["rev-parse", "HEAD:asset.bin"]);
        let mut invalid = old;
        invalid.size += 1;
        fs::write(root.join("asset.bin"), invalid.serialize()).unwrap();
        fs::write(root.join("ordinary.bin"), vec![0x80; 65536]).unwrap();
        commit(root);
        let cancel = CancellationToken::new();
        assert!(matches!(
            collect_push_pointers(root, true, &[], &cancel),
            Err(CrabError::LfsObjectCorrupt { .. })
        ));
        let large_blob = git(root, &["rev-parse", "HEAD:ordinary.bin"]);
        let object = |id: &str| root.join(".git/objects").join(&id[..2]).join(&id[2..]);
        let replacement = fs::read(object(&large_blob)).unwrap();
        fs::remove_file(object(&old_blob)).unwrap();
        fs::write(object(&old_blob), replacement).unwrap();
        assert!(
            matches!(collect_push_pointers(root, true, &[], &cancel), Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData && error.to_string().contains("checksum differs"))
        );
    }
}
