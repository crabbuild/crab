use super::*;

fn commit_tree(root: &Path, tree: &str, parents: &[&str]) -> String {
    let mut command = git_command_in(root, GitObjectAccess::LocalOnly);
    command.args([
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit-tree",
        tree,
        "-m",
        "previous version",
    ]);
    for parent in parents {
        command.args(["-p", parent]);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn store_pointer(root: &Path, pointer: &LfsPointer) -> String {
    std::fs::write(root.join("pointer"), pointer.serialize()).unwrap();
    let output = git_command_in(root, GitObjectAccess::LocalOnly)
        .args(["hash-object", "-w", "pointer"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn previous_parser_preserves_old_paths_and_skips_additions_and_gitlinks() {
    for length in [40, 64] {
        let old = "a".repeat(length);
        let new = "b".repeat(length);
        let zero = "0".repeat(length);
        for (header, expected) in [
            (format!(":100644 100755 {old} {new} M"), Some(old.clone())),
            (format!(":100644 000000 {old} {zero} D"), Some(old.clone())),
            (format!(":120000 100644 {old} {new} T"), Some(old.clone())),
            (format!(":000000 100644 {zero} {new} A"), None),
            (format!(":160000 160000 {old} {new} M"), None),
        ] {
            let mut state = PreviousObject::default();
            assert!(
                parse_previous_object(header.as_bytes(), &mut state)
                    .unwrap()
                    .is_none()
            );
            let path = "directory/line\nwith\ttabs.bin";
            let entry = parse_previous_object(path.as_bytes(), &mut state).unwrap();
            assert_eq!(entry, expected.map(|oid| (oid, path.to_owned())));
            state.finish().unwrap();
        }
    }
}

#[test]
fn previous_parser_rejects_malformed_headers_paths_and_truncated_records() {
    let old = git_oid(1);
    let new = git_oid(2);
    let zero = "0".repeat(40);
    for header in [
        String::new(),
        format!(":100644 100644 {old} {new}"),
        format!("::100644 100644 {old} {new} M"),
        format!(":999999 100644 {old} {new} M"),
        format!(":100644 100644 bad {new} M"),
        format!(":100644 100644 {old} {} M", "b".repeat(64)),
        format!(":100644 100644 {old} {new} R100"),
        format!(":100644 100644 {old} {new} X"),
        format!(":000000 100644 {old} {new} A"),
        format!(":100644 100644 {old} {zero} M"),
    ] {
        assert!(parse_previous_object(header.as_bytes(), &mut PreviousObject::default()).is_err());
    }
    let header = format!(":100644 100644 {old} {new} M");
    for path in [b"".as_slice(), &[0xff]] {
        let mut state = PreviousObject::default();
        parse_previous_object(header.as_bytes(), &mut state).unwrap();
        assert!(parse_previous_object(path, &mut state).is_err());
    }
    let cancel = CancellationToken::new();
    let incomplete = format!("{header}\0");
    let state = read_discovery(
        incomplete.as_bytes(),
        b'\0',
        parse_previous_object,
        &cancel,
        1024,
        |_| Ok(()),
    )
    .unwrap();
    assert!(state.finish().is_err());
    let truncated = format!("{header}\0asset");
    assert!(
        read_discovery(
            truncated.as_bytes(),
            b'\0',
            parse_previous_object,
            &cancel,
            1024,
            |_| Ok(())
        )
        .is_err()
    );
}

#[test]
fn previous_discovery_reuses_the_bounded_batch_reader() {
    let old = git_oid(1);
    let new = git_oid(2);
    let history = format!(":100644 100644 {old} {new} M\0asset\0").repeat(1000);
    let mut batches = Vec::new();
    let bytes = std::mem::size_of::<(String, String)>() + old.len() + "asset".len();
    let state = read_discovery(
        history.as_bytes(),
        b'\0',
        parse_previous_object,
        &CancellationToken::new(),
        (bytes * 2) as u64,
        |batch| {
            batches.push(batch.len());
            Ok(())
        },
    )
    .unwrap();
    state.finish().unwrap();
    assert_eq!(batches, vec![2; 500]);
}

#[test]
fn merge_history_reads_each_parent_without_recursing_into_unselected_commits() {
    let (dir, base_oid, base_pointer) = pointer_object_fixture();
    let root = dir.path();
    let mut pointers = vec![base_pointer];
    let base_tree = tree_with_blobs(root, &[(&base_oid, "asset.bin")]);
    let base = commit_tree(root, &base_tree, &[]);
    let mut trees = Vec::new();
    for byte in [5, 6, 7] {
        let pointer = LfsPointer {
            oid: [byte; 32],
            size: 42,
            extensions: Vec::new(),
        };
        let oid = store_pointer(root, &pointer);
        trees.push(tree_with_blobs(root, &[(&oid, "asset.bin")]));
        pointers.push(pointer);
    }
    let left = commit_tree(root, &trees[0], &[&base]);
    let right = commit_tree(root, &trees[1], &[&base]);
    let merge = commit_tree(root, &trees[2], &[&left, &right]);
    for mode in ["combined", "first-parent", "off"] {
        assert!(
            git_command_in(root, GitObjectAccess::LocalOnly)
                .args(["config", "log.diffMerges", mode])
                .status()
                .unwrap()
                .success()
        );
        let entries = collect_pointers_for_fetch_in(
            root,
            std::slice::from_ref(&merge),
            std::slice::from_ref(&merge),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            entries,
            [
                ("asset.bin".to_owned(), pointers[3].clone()),
                ("asset.bin".to_owned(), pointers[1].clone()),
                ("asset.bin".to_owned(), pointers[2].clone()),
            ],
            "merge display mode {mode}"
        );
    }
}

#[test]
fn renamed_previous_paths_survive_alias_deduplication_and_subdirectory_scans() {
    let (dir, oid, pointer) = pointer_object_fixture();
    let root = dir.path();
    let old_paths = ["line\nname.bin", "tabs\tname.bin"];
    let old_tree = tree_with_blobs(root, &old_paths.map(|path| (oid.as_str(), path)));
    let old = commit_tree(root, &old_tree, &[]);
    let new_tree = tree_with_blobs(root, &[(&oid, "renamed.bin")]);
    let new = commit_tree(root, &new_tree, &[&old]);
    let subdir = root.join("subdir");
    std::fs::create_dir(&subdir).unwrap();
    for args in [
        ["config", "diff.renames", "true"],
        ["config", "diff.relative", "true"],
    ] {
        assert!(
            git_command_in(root, GitObjectAccess::LocalOnly)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let entries = collect_pointers_for_fetch_in(
        &subdir,
        std::slice::from_ref(&new),
        std::slice::from_ref(&new),
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(
        entries,
        ["renamed.bin", old_paths[0], old_paths[1]].map(|path| (path.to_owned(), pointer.clone()))
    );
}

#[test]
fn conflicting_historical_sizes_invalidate_the_entire_fetch_inventory() {
    let (dir, old_oid, mut pointer) = pointer_object_fixture();
    let root = dir.path();
    let old_tree = tree_with_blobs(root, &[(&old_oid, "asset.bin")]);
    let old = commit_tree(root, &old_tree, &[]);
    pointer.size += 1;
    let current = store_pointer(root, &pointer);
    let new_tree = tree_with_blobs(root, &[(&current, "asset.bin")]);
    let new = commit_tree(root, &new_tree, &[&old]);
    assert!(matches!(
        collect_pointers_for_fetch_in(
            root,
            std::slice::from_ref(&new),
            std::slice::from_ref(&new),
            &CancellationToken::new()
        ),
        Err(CrabError::LfsObjectCorrupt { .. })
    ));
}

#[test]
fn missing_or_corrupt_previous_blobs_do_not_return_current_only_success() {
    let (dir, old_oid, mut pointer) = pointer_object_fixture();
    let root = dir.path();
    let old_tree = tree_with_blobs(root, &[(&old_oid, "asset.bin")]);
    let old = commit_tree(root, &old_tree, &[]);
    pointer.oid = [5; 32];
    let current = store_pointer(root, &pointer);
    let new_tree = tree_with_blobs(root, &[(&current, "asset.bin")]);
    let new = commit_tree(root, &new_tree, &[&old]);
    let object = |oid: &str| root.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
    let replacement = std::fs::read(object(&current)).unwrap();
    std::fs::remove_file(object(&old_oid)).unwrap();
    let cancel = CancellationToken::new();
    assert!(
        collect_pointers_for_fetch_in(
            root,
            std::slice::from_ref(&new),
            std::slice::from_ref(&new),
            &cancel
        )
        .is_err()
    );
    std::fs::write(object(&old_oid), replacement).unwrap();
    assert!(
        matches!(collect_pointers_for_fetch_in(root, std::slice::from_ref(&new),
        std::slice::from_ref(&new), &cancel), Err(CrabError::Io(error))
        if error.kind() == io::ErrorKind::InvalidData && error.to_string().contains("checksum differs"))
    );
}

#[test]
fn previous_discovery_verifies_sha256_git_objects() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(
        git_command_in(root, GitObjectAccess::LocalOnly)
            .args(["init", "-q", "--object-format=sha256"])
            .status()
            .unwrap()
            .success()
    );
    let old_pointer = LfsPointer {
        oid: [4; 32],
        size: 42,
        extensions: Vec::new(),
    };
    let old_oid = store_pointer(root, &old_pointer);
    let old_tree = tree_with_blobs(root, &[(&old_oid, "asset.bin")]);
    let old = commit_tree(root, &old_tree, &[]);
    let empty_tree = tree_with_blobs(root, &[]);
    let deleted = commit_tree(root, &empty_tree, &[&old]);
    let entries =
        previous_pointers_in_commits(root, &[deleted], &CancellationToken::new()).unwrap();
    assert_eq!(entries, [("asset.bin".to_owned(), old_pointer)]);
}

#[test]
fn previous_scan_requires_frozen_existing_commits_and_honors_precancellation() {
    let (dir, _, _) = pointer_object_fixture();
    let cancel = CancellationToken::new();
    for commit in ["HEAD".to_owned(), "--all".to_owned(), git_oid(17)] {
        assert!(previous_pointers_in_commits(dir.path(), &[commit], &cancel).is_err());
    }
    cancel.cancel();
    assert!(matches!(
        previous_pointers_in_commits(Path::new("absent"), &[], &cancel),
        Err(CrabError::Cancelled)
    ));
}
