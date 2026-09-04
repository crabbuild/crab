use super::*;

fn oid(byte: u8, width: usize) -> String {
    format!("{byte:02x}").repeat(width / 2)
}

fn update(local: &str, new: &str, remote: &str, old: &str) -> String {
    format!("{local} {new} {remote} {old}\n")
}

#[test]
fn complete_batch_keeps_exact_oids_and_remote_names_in_input_order() {
    let new = oid(1, 40);
    let old = oid(2, 40);
    let zero = oid(0, 40);
    let input = [
        update("HEAD~", &new, "refs/heads/release", &old),
        update("refs/tags/v1", &old, "refs/tags/published", &zero),
        update("(delete)", &zero, "refs/heads/retired", &old),
        update(&old, &old, "refs/notes/build", &zero),
    ]
    .concat();
    let updates = read_pre_push(input.as_bytes(), 4096).unwrap();
    assert_eq!(
        updates,
        vec![
            PrePushUpdate {
                local_oid: Some(new),
                remote_ref: "refs/heads/release".into(),
                remote_oid: Some(old.clone()),
            },
            PrePushUpdate {
                local_oid: Some(old.clone()),
                remote_ref: "refs/tags/published".into(),
                remote_oid: None,
            },
            PrePushUpdate {
                local_oid: None,
                remote_ref: "refs/heads/retired".into(),
                remote_oid: Some(old.clone()),
            },
            PrePushUpdate {
                local_oid: Some(old),
                remote_ref: "refs/notes/build".into(),
                remote_oid: None,
            },
        ]
    );
}

#[test]
fn malformed_later_record_rejects_the_whole_batch() {
    let new = oid(1, 40);
    let zero = oid(0, 40);
    let valid = update("main", &new, "refs/heads/main", &zero);
    let bad_records = [
        "\n".to_owned(),
        "main only-three fields\n".to_owned(),
        format!("main {new} refs/heads/next {zero} extra\n"),
        update("main", "0123", "refs/heads/next", &zero),
        update("main", &"x".repeat(40), "refs/heads/next", &zero),
        update("main", &new, "refs/heads/next", &oid(0, 64)),
        update("main", &oid(1, 64), "refs/heads/next", &oid(0, 64)),
        update("main", &new, "next", &zero),
        update("main", &new, "refs/heads/../bad", &zero),
        update("main", &new, "refs/heads/main", &zero),
        update("main", &zero, "refs/heads/next", &new),
        update("(delete)", &new, "refs/heads/next", &zero),
        update("main\0", &new, "refs/heads/next", &zero),
        format!("main {new} refs/heads/next {zero}"),
    ];
    for bad in bad_records {
        let input = format!("{valid}{bad}");
        let error = read_pre_push(input.as_bytes(), 4096).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{bad:?}");
    }
}

#[test]
fn invalid_ref_keeps_the_validation_error_as_its_source() {
    let input = update("main", &oid(1, 40), "refs/heads/x.lock", &oid(0, 40));
    let error = read_pre_push(input.as_bytes(), 4096).unwrap_err();
    assert!(
        error
            .get_ref()
            .unwrap()
            .is::<crate::refname::RefNameError>()
    );
}

#[test]
fn non_utf8_input_is_rejected_without_lossy_ref_names() {
    let mut input = update("main", &oid(1, 40), "refs/heads/name", &oid(0, 40)).into_bytes();
    input[0] = 0xff;
    assert_eq!(
        read_pre_push(input.as_slice(), 4096).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn same_format_sha256_batch_decodes_without_claiming_transport_support() {
    let uppercase_oid = oid(0xab, 64).to_ascii_uppercase();
    let input = update("HEAD~", &uppercase_oid, "refs/heads/main", &oid(0, 64));
    let updates = read_pre_push(input.as_bytes(), 4096).unwrap();
    assert_eq!(updates[0].local_oid, Some(oid(0xab, 64)));
}

#[test]
fn empty_batch_is_a_noop_at_zero_limit() {
    assert!(read_pre_push(io::empty(), 0).unwrap().is_empty());
}

#[test]
fn exact_size_batch_is_accepted_and_one_byte_more_is_rejected() {
    let input = update("main", &oid(1, 40), "refs/heads/main", &oid(0, 40));
    let limit = input.len() as u64;
    for (size, accepted) in [(limit, true), (limit - 1, false)] {
        assert_eq!(read_pre_push(input.as_bytes(), size).is_ok(), accepted);
    }
}

#[test]
fn oversized_stream_reads_only_limit_plus_one_bytes() {
    let mut input = io::Cursor::new(vec![b'x'; 1024]);
    let error = read_pre_push(&mut input, 8).unwrap_err();
    assert_eq!(
        (error.kind(), input.position()),
        (io::ErrorKind::InvalidData, 9)
    );
}

#[test]
fn input_failure_does_not_return_a_successful_prefix() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "input lost"))
        }
    }
    let valid = update("main", &oid(1, 40), "refs/heads/main", &oid(0, 40));
    let input = valid.as_bytes().chain(Broken);
    assert_eq!(
        read_pre_push(input, 4096).unwrap_err().kind(),
        io::ErrorKind::ConnectionReset
    );
}

#[cfg(unix)]
#[test]
fn real_git_multi_ref_push_decodes_mapping_tag_rewrite_and_deletion() {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_CONFIG",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
        ] {
            command.env_remove(key);
        }
        let output = command
            .args([
                "-c",
                "user.name=Hook Test",
                "-c",
                "user.email=hook@example.invalid",
            ])
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let remote = dir.path().join("remote.git");
    git(dir.path(), &["init", "--bare", remote.to_str().unwrap()]);
    git(
        dir.path(),
        &["init", "-b", "main", source.to_str().unwrap()],
    );
    git(
        &source,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&source, &["commit", "--allow-empty", "-m", "first"]);
    let first = git(&source, &["rev-parse", "HEAD"]);
    git(&source, &["push", "origin", "HEAD:refs/heads/deleted"]);
    git(&source, &["commit", "--allow-empty", "-m", "second"]);
    let second = git(&source, &["rev-parse", "HEAD"]);
    git(&source, &["push", "origin", "HEAD:refs/heads/published"]);
    git(&source, &["tag", "-a", "v1", "-m", "annotated"]);
    let tag = git(&source, &["rev-parse", "refs/tags/v1"]);

    let hook = source.join(".git/hooks/pre-push");
    std::fs::write(&hook, "#!/bin/sh\ncat > .git/pre-push-input\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &source,
        &[
            "push",
            "--atomic",
            "origin",
            "HEAD~:refs/heads/revision",
            "refs/heads/main:refs/heads/renamed",
            "refs/tags/v1:refs/tags/released",
            "+HEAD~:refs/heads/published",
            ":refs/heads/deleted",
        ],
    );

    let file = std::fs::File::open(source.join(".git/pre-push-input")).unwrap();
    let actual: BTreeMap<_, _> = read_pre_push(file, 4096)
        .unwrap()
        .into_iter()
        .map(|entry| (entry.remote_ref, (entry.local_oid, entry.remote_oid)))
        .collect();
    let expected = BTreeMap::from([
        (
            "refs/heads/revision".to_owned(),
            (Some(first.clone()), None),
        ),
        (
            "refs/heads/renamed".to_owned(),
            (Some(second.clone()), None),
        ),
        ("refs/tags/released".to_owned(), (Some(tag.clone()), None)),
        (
            "refs/heads/published".to_owned(),
            (Some(first.clone()), Some(second)),
        ),
        ("refs/heads/deleted".to_owned(), (None, Some(first))),
    ]);
    assert_eq!(actual, expected);

    let visible = git(&remote, &["show-ref"]);
    let visible: BTreeMap<_, _> = visible
        .lines()
        .map(|line| {
            let (oid, name) = line.split_once(' ').unwrap();
            (name.to_owned(), oid.to_owned())
        })
        .collect();
    let expected_visible = expected
        .into_iter()
        .filter_map(|(name, (oid, _))| oid.map(|oid| (name, oid)))
        .collect();
    assert_eq!(visible, expected_visible);
}
