use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn command(root: &Path, binary: &str, args: &[&str]) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("CRAB_CACHE_DIR", root.join("cache"));
    command
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = command(root, "git", args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn fixture() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "--initial-branch=main"]);
    let driver = format!(
        "'{}' filter-process",
        env!("CARGO_BIN_EXE_crab").replace('\'', "'\\''")
    );
    for kind in ["crab", "lfs"] {
        git(
            root,
            &["config", &format!("filter.{kind}.process"), &driver],
        );
        git(
            root,
            &["config", &format!("filter.{kind}.required"), "true"],
        );
    }
    std::fs::write(
        root.join(".gitattributes"),
        b"*.bin filter=crab -text\n*.lfs filter=lfs -text\n",
    )
    .unwrap();
    directory
}

fn read(root: &Path, pointer: &[u8], offset: u64, length: u32) -> Output {
    read_with_args(
        root,
        pointer,
        &[
            "read-pointer",
            "--offset",
            &offset.to_string(),
            "--length",
            &length.to_string(),
        ],
    )
}

fn read_with_args(root: &Path, pointer: &[u8], args: &[&str]) -> Output {
    let mut child = command(root, env!("CARGO_BIN_EXE_crab"), args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(pointer).unwrap();
    drop(stdin);
    child.wait_with_output().unwrap()
}

fn content() -> Vec<u8> {
    (0..2 * 1024 * 1024_usize)
        .map(|i| ((i.wrapping_mul(73) ^ (i >> 8)) & 255) as u8)
        .collect()
}

#[test]
fn staged_crab_and_lfs_ranges_are_byte_identical_without_hydrating_worktree() {
    let directory = fixture();
    let root = directory.path();
    let original = content();
    for path in ["file.bin", "file.lfs"] {
        std::fs::write(root.join(path), &original).unwrap();
        git(root, &["add", "--", path]);
        let pointer = git(root, &["show", &format!(":{path}")]);
        assert!(!matches!(
            crab_git::classify(&pointer),
            crab_git::PointerKind::NotAPointer
        ));
        std::fs::write(root.join(path), &pointer).unwrap();
        for (offset, length) in [
            (0, 8),
            (65530, 65547),
            (original.len() as u64 - 9, 100),
            (u64::MAX, 100),
        ] {
            let output = read(root, &pointer, offset, length);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let start = offset.min(original.len() as u64) as usize;
            let end = start.saturating_add(length as usize).min(original.len());
            assert_eq!(output.stdout, original[start..end]);
        }
        assert_eq!(std::fs::read(root.join(path)).unwrap(), pointer);
    }
}

#[test]
fn corrupt_lfs_payload_never_returns_a_successful_partial_preview() {
    let directory = fixture();
    let root = directory.path();
    std::fs::write(root.join("file.lfs"), content()).unwrap();
    git(root, &["add", "file.lfs"]);
    let pointer = git(root, &["show", ":file.lfs"]);
    let pointer_data = crab_git::LfsPointer::parse(&pointer).unwrap();
    let storage = crab_lfs::local::storage_from_git(&root.join(".git")).unwrap();
    let object = crab_lfs::local::object_path(&storage, &pointer_data.oid);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(object)
        .unwrap();
    file.write_all(b"corrupt").unwrap();
    file.sync_all().unwrap();
    let output = read(root, &pointer, 1024 * 1024, 8);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn pointer_context_capture_rejects_a_changed_remote_before_storage_access() {
    let directory = fixture();
    let root = directory.path();
    let pointer = crab_types::pointer::Pointer {
        file_hash: [1; 32],
        size: 32,
        shard_hint: None,
    }
    .serialize();
    let inspected = read_with_args(root, &pointer, &["read-pointer", "--inspect"]);
    assert!(
        inspected.status.success(),
        "{}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let configuration: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    let expected = serde_json::to_string(&configuration["remote_url"]).unwrap();
    std::fs::write(
        root.join("crab.toml"),
        "version = 1\n[remote]\nurl = 'crab://changed.invalid/repository'\n",
    )
    .unwrap();
    let output = read_with_args(
        root,
        &pointer,
        &[
            "read-pointer",
            "--offset",
            "0",
            "--length",
            "8",
            "--expected-remote",
            &expected,
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("content repository changed"));
    assert!(output.stdout.is_empty());
}
