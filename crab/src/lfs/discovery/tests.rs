use super::*;
use sha2::Digest;
use std::process::Stdio;

fn git_oid(byte: u8) -> String {
    format!("{byte:02x}").repeat(20)
}

fn tree_with_blobs(root: &Path, objects: &[(&str, &str)]) -> String {
    let records = objects
        .iter()
        .map(|(oid, name)| format!("100644 blob {oid}\t{name}\0"))
        .collect::<String>();
    let mut child = git_command_in(root, GitObjectAccess::LocalOnly)
        .args(["mktree", "-z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(records.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn tree_scan_preserves_aliases_and_nul_framed_path_names() {
    let (dir, oid, pointer) = pointer_object_fixture();
    let names = ["a.bin", "line\nname.bin", "z.bin"];
    let tree = tree_with_blobs(dir.path(), &names.map(|name| (oid.as_str(), name)));
    let entries = collect_pointers_from_trees_in(
        dir.path(),
        &[tree.clone(), tree],
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(
        entries,
        names.map(|name| (name.to_owned(), pointer.clone()))
    );
}

#[test]
fn tree_scan_rejects_conflicting_sizes_across_aliases() {
    let (dir, oid, mut pointer) = pointer_object_fixture();
    pointer.size += 1;
    std::fs::write(dir.path().join("other.bin"), pointer.serialize()).unwrap();
    let output = git_command_in(dir.path(), GitObjectAccess::LocalOnly)
        .args(["hash-object", "-w", "other.bin"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let other = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    let tree = tree_with_blobs(dir.path(), &[(&oid, "a.bin"), (&other, "b.bin")]);
    let result = collect_pointers_from_trees_in(dir.path(), &[tree], &CancellationToken::new());
    assert!(matches!(result, Err(CrabError::LfsObjectCorrupt { .. })));
}

#[test]
fn rev_list_parser_accepts_line_delimited_named_objects() {
    let mut pending = None;
    let unnamed = git_oid(4);
    assert!(
        parse_rev_list_record(unnamed.as_bytes(), &mut pending)
            .unwrap()
            .is_none()
    );

    let named = git_oid(5);
    let record = format!("{named} path with spaces");
    assert_eq!(
        parse_rev_list_record(record.as_bytes(), &mut pending).unwrap(),
        Some((named, "path with spaces".to_owned()))
    );
}

#[test]
fn range_scan_does_not_fall_back_when_rev_list_fails() {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .current_dir(dir.path())
        .args(["init", "--quiet"])
        .status()
        .unwrap();
    assert!(status.success());

    let error = collect_pointers_from_range_in(
        dir.path(),
        &["not-a-git-object".to_owned()],
        &[],
        &CancellationToken::new(),
    )
    .unwrap_err();

    assert!(format!("{error}").contains("git rev-list failed"));
}

#[test]
fn range_scan_streams_line_delimited_rev_list_and_cat_file_records() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    let content = b"streamed lfs object";
    let pointer = LfsPointer {
        oid: sha2::Sha256::digest(content).into(),
        size: content.len() as u64,
        extensions: Vec::new(),
    };
    std::fs::write(dir.path().join("asset.bin"), pointer.serialize()).unwrap();
    std::fs::write(
        dir.path().join("large.bin"),
        vec![b'x'; MAX_LFS_POINTER_SIZE + 1024],
    )
    .unwrap();
    assert!(
        Command::new("git")
            .current_dir(dir.path())
            .args(["add", "."])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .current_dir(dir.path())
            .args(["commit", "--quiet", "-m", "fixture"])
            .status()
            .unwrap()
            .success()
    );
    let head = String::from_utf8(
        Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();

    let entries =
        collect_pointers_from_range_in(dir.path(), &[head], &[], &CancellationToken::new())
            .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "asset.bin");
    assert_eq!(entries[0].1, pointer);
}

#[test]
fn range_scan_excludes_all_base_manifest_ref_tips() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    let base_content = b"base lfs object";
    let base_pointer = LfsPointer {
        oid: sha2::Sha256::digest(base_content).into(),
        size: base_content.len() as u64,
        extensions: Vec::new(),
    };
    std::fs::write(dir.path().join("base.bin"), base_pointer.serialize()).unwrap();
    assert!(
        Command::new("git")
            .current_dir(dir.path())
            .args(["add", "base.bin"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .current_dir(dir.path())
            .args(["commit", "--quiet", "-m", "base"])
            .status()
            .unwrap()
            .success()
    );
    let base = String::from_utf8(
        Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();

    let new_content = b"new lfs object";
    let new_pointer = LfsPointer {
        oid: sha2::Sha256::digest(new_content).into(),
        size: new_content.len() as u64,
        extensions: Vec::new(),
    };
    std::fs::write(dir.path().join("new.bin"), new_pointer.serialize()).unwrap();
    assert!(
        Command::new("git")
            .current_dir(dir.path())
            .args(["add", "new.bin"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .current_dir(dir.path())
            .args(["commit", "--quiet", "-m", "new"])
            .status()
            .unwrap()
            .success()
    );
    let head = String::from_utf8(
        Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();

    let entries = collect_pointers_from_range_in_with_base_refs(
        dir.path(),
        &[head],
        &[],
        &[base],
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(entries, vec![("new.bin".to_owned(), new_pointer)]);
}

#[test]
fn cat_file_batch_stdout_handles_large_object_lists() {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .current_dir(dir.path())
        .args(["init", "--quiet"])
        .status()
        .unwrap();
    assert!(status.success());

    let blob_dir = dir.path().join("blobs");
    std::fs::create_dir(&blob_dir).unwrap();
    let mut paths = String::new();
    for index in 0..2048 {
        let file_name = format!("blob-{index:04}.txt");
        let path = blob_dir.join(&file_name);
        std::fs::write(path, format!("blob-{index:04}-{}\n", "x".repeat(256))).unwrap();
        paths.push_str("blobs/");
        paths.push_str(&file_name);
        paths.push('\n');
    }

    let mut child = Command::new("git")
        .current_dir(dir.path())
        .args(["hash-object", "-w", "--stdin-paths"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(paths.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());

    let object_ids = String::from_utf8(output.stdout).unwrap();
    let mut cat_file_input = object_ids.trim_end().to_owned();
    cat_file_input.push('\n');
    assert!(cat_file_input.len() > 64 * 1024);

    let stdout = cat_file_batch_stdout(dir.path(), cat_file_input).unwrap();
    let text = String::from_utf8_lossy(&stdout);
    assert_eq!(text.matches(" blob ").count(), 2048);
}

#[test]
fn repository_commands_clear_remote_helper_git_context() {
    let command = git_command_in(Path::new("repo/.git"), GitObjectAccess::LocalOnly);
    let overrides: std::collections::HashMap<_, _> = command.get_envs().collect();

    assert_eq!(overrides.get(std::ffi::OsStr::new("GIT_DIR")), Some(&None));
    assert_eq!(
        overrides.get(std::ffi::OsStr::new("GIT_WORK_TREE")),
        Some(&None)
    );
    assert_eq!(
        overrides.get(std::ffi::OsStr::new("GIT_COMMON_DIR")),
        Some(&None)
    );
}

#[test]
fn inspection_forbids_transports_while_fetch_inherits_caller_restrictions() {
    for (access, restricted) in [
        (GitObjectAccess::LocalOnly, true),
        (GitObjectAccess::PromisorAllowed, false),
    ] {
        let command = git_command_in(Path::new("repo"), access);
        let overrides: HashMap<_, _> = command.get_envs().collect();
        for (key, value) in [("GIT_NO_LAZY_FETCH", "1"), ("GIT_ALLOW_PROTOCOL", "")] {
            let expected = restricted.then_some(Some(std::ffi::OsStr::new(value)));
            assert_eq!(overrides.get(std::ffi::OsStr::new(key)), expected.as_ref());
        }
    }
}

#[test]
fn discovery_rejects_truncation_budget_exhaustion_and_cancellation() {
    let record = format!("{} asset.bin\n", git_oid(3));
    let read = |bytes: &[u8], budget, cancel: &CancellationToken| {
        let mut entries = Vec::new();
        read_discovery(
            bytes,
            b'\n',
            parse_rev_list_record,
            cancel,
            budget,
            |batch| {
                entries.extend_from_slice(batch);
                Ok(())
            },
        )?;
        Ok(entries)
    };
    let cancel = CancellationToken::new();
    assert_eq!(read(record.as_bytes(), 1024, &cancel).unwrap().len(), 1);
    assert!(read(record.trim_end().as_bytes(), 1024, &cancel).is_err());
    assert!(read(record.as_bytes(), record.len() as u64 - 1, &cancel).is_err());
    assert!(read(&vec![b'x'; 1024 * 1024 + 1], MAX_CAPTURE_BYTES, &cancel).is_err());
    cancel.cancel();
    assert!(matches!(
        read(record.as_bytes(), 1024, &cancel),
        Err(CrabError::Cancelled)
    ));
}

#[test]
fn discovery_streams_history_larger_than_its_retained_batch_budget() {
    let record = format!("{} asset.bin\n", git_oid(3));
    let bytes = std::mem::size_of::<(String, String)>() + 40 + "asset.bin".len();
    let input = record.repeat(100);
    let mut batch_sizes = Vec::new();
    read_discovery(
        input.as_bytes(),
        b'\n',
        parse_rev_list_record,
        &CancellationToken::new(),
        (bytes * 3) as u64,
        |batch| {
            batch_sizes.push(batch.len());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(batch_sizes, [vec![3; 33], vec![1]].concat());
}

#[test]
fn discovery_stops_before_admitting_more_records_after_batch_failure() {
    let record = format!("{} asset.bin\n", git_oid(3));
    let input = record.repeat(10);
    let mut batches = 0;
    let result = read_discovery(
        input.as_bytes(),
        b'\n',
        parse_rev_list_record,
        &CancellationToken::new(),
        128,
        |_| {
            batches += 1;
            Err(CrabError::Cancelled)
        },
    );
    assert!(matches!(result, Err(CrabError::Cancelled)));
    assert_eq!(batches, 1);
}

fn pointer_object_fixture() -> (tempfile::TempDir, String, LfsPointer) {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        git_command_in(dir.path(), GitObjectAccess::LocalOnly)
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success()
    );
    let pointer = LfsPointer {
        oid: [4; 32],
        size: 42,
        extensions: Vec::new(),
    };
    std::fs::write(dir.path().join("asset.bin"), pointer.serialize()).unwrap();
    let output = git_command_in(dir.path(), GitObjectAccess::LocalOnly)
        .args(["hash-object", "-w", "asset.bin"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let oid = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    (dir, oid, pointer)
}

#[test]
fn pointer_inventory_budget_is_shared_across_discovery_batches() {
    let (dir, oid, pointer) = pointer_object_fixture();
    let records = [(oid, "asset.bin".to_owned())];
    let mut remaining = pointer_memory("asset.bin", &pointer) as u64;
    let cancel = CancellationToken::new();
    assert_eq!(
        read_lfs_batch(
            dir.path(),
            &records,
            GitObjectAccess::LocalOnly,
            &cancel,
            &mut remaining
        )
        .unwrap(),
        [("asset.bin".to_owned(), pointer)]
    );
    assert!(matches!(
        read_lfs_batch(dir.path(), &records, GitObjectAccess::LocalOnly, &cancel, &mut remaining),
        Err(CrabError::Io(error)) if error.kind() == io::ErrorKind::InvalidData
    ));
}

#[test]
fn range_scans_verify_sha1_and_sha256_and_reject_disguised_pointer_bodies() {
    for format in ["sha1", "sha256"] {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = git_command_in(dir.path(), GitObjectAccess::LocalOnly)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--object-format={format}"),
        ]);
        let pointer = LfsPointer {
            oid: [4; 32],
            size: 42,
            extensions: Vec::new(),
        };
        std::fs::write(dir.path().join("asset.bin"), pointer.serialize()).unwrap();
        std::fs::write(dir.path().join("ordinary.bin"), vec![0x80; 65536]).unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "LFS scan",
        ]);
        let head = git(&["rev-parse", "HEAD"]);
        let cancel = CancellationToken::new();
        let entries =
            collect_pointers_from_range_in(dir.path(), std::slice::from_ref(&head), &[], &cancel)
                .unwrap();
        assert_eq!(entries, [("asset.bin".to_owned(), pointer)]);

        let oid = git(&["rev-parse", "HEAD:asset.bin"]);
        let large = git(&["rev-parse", "HEAD:ordinary.bin"]);
        let object = |id: &str| {
            dir.path()
                .join(".git/objects")
                .join(&id[..2])
                .join(&id[2..])
        };
        let replacement = std::fs::read(object(&large)).unwrap();
        std::fs::remove_file(object(&oid)).unwrap();
        std::fs::write(object(&oid), replacement).unwrap();
        let error =
            collect_pointers_from_range_in(dir.path(), std::slice::from_ref(&head), &[], &cancel)
                .unwrap_err();
        assert!(
            matches!(error, CrabError::Io(ref error) if error.kind() == io::ErrorKind::InvalidData
            && error.to_string().contains("checksum differs"))
        );
        let error =
            collect_pointers_from_trees_in(dir.path(), std::slice::from_ref(&head), &cancel)
                .unwrap_err();
        assert!(matches!(error, CrabError::Io(error)
            if error.kind() == io::ErrorKind::InvalidData
            && error.to_string().contains("checksum differs")));
        assert_eq!(git(&["rev-parse", "HEAD"]), head);
    }
}

#[test]
fn cancelled_range_scan_does_not_open_a_repository() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        collect_pointers_from_range_in(Path::new("absent"), &[git_oid(4)], &[], &cancel),
        Err(CrabError::Cancelled)
    ));
}

#[cfg(unix)]
#[test]
fn discovery_drains_stderr_and_preserves_a_nonzero_exit() {
    let (dir, oid, _) = pointer_object_fixture();
    let cancel = CancellationToken::new();
    let timeout = cancel.clone();
    let (done, receiver) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if receiver
            .recv_timeout(std::time::Duration::from_secs(15))
            .is_err()
        {
            timeout.cancel();
        }
    });
    let args = vec![
        "-c".into(),
        format!(
            "alias.discovery=!printf '%s asset.bin\\n' {oid}; head -c 131072 /dev/zero >&2; exit 7"
        ),
        "discovery".into(),
    ];
    let result = visit_lfs_blobs_in_git_command(
        dir.path(),
        &args,
        b'\n',
        parse_rev_list_record,
        GitObjectAccess::LocalOnly,
        &cancel,
        |_, _| panic!("failed discovery must not emit pointers"),
    );
    let _ = done.send(());
    watchdog.join().unwrap();
    assert!(
        matches!(result, Err(CrabError::Internal(message)) if message.starts_with("git -c failed:"))
    );
}

#[cfg(unix)]
#[test]
fn stalled_discovery_cancels_before_cache_ownership_returns() {
    use crab_cache::lifecycle::CacheUseGuard;
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache.git");
    let ready = dir.path().join("ready");
    let cancel = CancellationToken::new();
    std::thread::scope(|scope| {
        let worker_cancel = cancel.clone();
        let root = dir.path();
        let cache = &cache;
        let worker = scope.spawn(move || {
            let _owner = CacheUseGuard::acquire(cache, &worker_cancel).unwrap();
            let args = vec![
                "-c".into(),
                format!(
                    "alias.discovery=!printf '%s asset.bin\\n' {}; touch ready; sleep 30",
                    git_oid(4)
                ),
                "discovery".into(),
            ];
            visit_lfs_blobs_in_git_command(
                root,
                &args,
                b'\n',
                parse_rev_list_record,
                GitObjectAccess::LocalOnly,
                &worker_cancel,
                |_, _| panic!("incomplete discovery must not emit pointers"),
            )
        });
        let start = Instant::now();
        while !ready.exists() {
            if start.elapsed() >= Duration::from_secs(15) {
                cancel.cancel();
                panic!("discovery fixture did not start");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(CacheUseGuard::acquire(cache, &CancellationToken::new()).is_err());
        cancel.cancel();
        assert!(matches!(worker.join().unwrap(), Err(CrabError::Cancelled)));
    });
    CacheUseGuard::acquire(&cache, &CancellationToken::new()).unwrap();
}

fn cat_file_batch_stdout(repo_dir: &Path, oids_input: String) -> Result<Vec<u8>> {
    let mut command = git_command_in(repo_dir, GitObjectAccess::LocalOnly);
    command.args(["cat-file", "--batch"]);
    let output = process::run(
        command,
        &CancellationToken::new(),
        Some(|mut stdin: ChildStdin| {
            stdin.write_all(oids_input.as_bytes())?;
            Ok(())
        }),
        |stdout| Ok(process::capture_output(stdout, MAX_CAPTURE_BYTES)?),
    )?;
    successful_git("cat-file", output)
}
