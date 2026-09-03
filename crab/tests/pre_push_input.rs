//! Admission proof through the real CLI, without a repository or cloud setup.

use std::io::{Seek, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn pre_push(root: &Path, input: impl Into<Stdio>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crab"))
        .args(["lfs", "pre-push"])
        .current_dir(root)
        .env("CRAB_CACHE_DIR", root.join("isolated-cache"))
        .stdin(input)
        .output()
        .unwrap()
}

fn input_file(root: &Path, input: &[u8]) -> std::fs::File {
    let mut file = tempfile::tempfile_in(root).unwrap();
    file.write_all(input).unwrap();
    file.rewind().unwrap();
    file
}

#[test]
fn empty_and_delete_only_batches_need_no_cloud_or_git_repository() {
    let dir = tempfile::tempdir().unwrap();
    let deletion = format!(
        "(delete) {} refs/heads/retired {}\n",
        "0".repeat(40),
        "1".repeat(40)
    );
    for input in [b"".as_slice(), deletion.as_bytes()] {
        let result = pre_push(dir.path(), input_file(dir.path(), input));
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn invalid_later_update_fails_before_remote_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let update = format!(
        "HEAD~ {} refs/heads/release {}\n",
        "1".repeat(40),
        "0".repeat(40)
    );
    let input = update.repeat(2);
    let result = pre_push(dir.path(), input_file(dir.path(), input.as_bytes()));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "{stderr}");
    assert!(stderr.contains("duplicate destination ref"), "{stderr}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn oversized_batch_is_rejected_by_the_cli_admission_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut input = tempfile::tempfile_in(dir.path()).unwrap();
    input.write_all(&vec![b'x'; 16 * 1024 * 1024 + 1]).unwrap();
    input.rewind().unwrap();
    let result = pre_push(dir.path(), input);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "{stderr}");
    assert!(
        stderr.contains("pre-push input exceeds 16777216 bytes"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn direct_crab_batch_does_not_require_mirror_configuration_or_recurse() {
    let dir = tempfile::tempdir().unwrap();
    let input = format!(
        "main {} refs/heads/main {}\n",
        "1".repeat(40),
        "0".repeat(40)
    );
    let output = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args(["mirror-pre-push", "crab", "crab://bucket/repo"])
        .current_dir(dir.path())
        .stdin(input_file(dir.path(), input.as_bytes()))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
