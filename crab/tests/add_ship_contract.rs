use std::path::Path;
use std::process::{Command, Output};

fn crab(repo: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crab"))
        .args(args)
        .current_dir(repo)
        .env("CRAB_CACHE_DIR", repo.join(".crab/test-cache"))
        .output()
        .expect("run crab")
}

fn git(repo: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git")
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_success(&git(dir.path(), &["init", "-q", "-b", "main"]), "git init");
    assert_success(
        &git(dir.path(), &["config", "user.name", "Crab Contract Test"]),
        "git config user.name",
    );
    assert_success(
        &git(
            dir.path(),
            &["config", "user.email", "contract-test@crab.local"],
        ),
        "git config user.email",
    );
    assert_success(
        &crab(dir.path(), &["init", "crab://contract-test/repo"]),
        "crab init",
    );
    assert_success(
        &git(dir.path(), &["commit", "-qm", "initialize crab"]),
        "initial commit",
    );
    dir
}

fn write_large_model(repo: &Path) {
    std::fs::write(repo.join("model.bin"), vec![7_u8; 1024 * 1024 + 1]).expect("write model");
}

#[test]
fn standalone_add_stages_generated_tracking_metadata() {
    let dir = repository();
    write_large_model(dir.path());

    let output = crab(dir.path(), &["add", "model.bin", "--json", "--jobs", "1"]);
    assert_success(&output, "crab add");
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).expect("add JSON");
    assert_eq!(envelope["schema"], "add");

    let status = git(dir.path(), &["status", "--short"]);
    assert_success(&status, "git status");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        "A  .gitattributes\nA  model.bin\n"
    );

    assert_success(
        &git(dir.path(), &["commit", "-qm", "add model"]),
        "model commit",
    );
    let tree = git(dir.path(), &["ls-tree", "--name-only", "HEAD"]);
    assert_success(&tree, "git ls-tree");
    let tree = String::from_utf8_lossy(&tree.stdout);
    assert!(tree.lines().any(|path| path == ".gitattributes"));
    assert!(tree.lines().any(|path| path == "model.bin"));
}

#[test]
fn ship_json_emits_one_terminal_envelope() {
    let dir = repository();
    write_large_model(dir.path());

    let output = crab(
        dir.path(),
        &[
            "ship",
            "model.bin",
            "--message",
            "ship model",
            "--no-push",
            "--json",
            "--jobs",
            "1",
        ],
    );
    assert_success(&output, "crab ship");
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).expect("ship JSON");
    assert_eq!(envelope["schema"], "ship");
    assert_eq!(envelope["data"]["dry_run"], false);
    assert_eq!(envelope["data"]["committed"], true);
    assert_eq!(envelope["data"]["add"]["files_staged"], 1);
    assert!(envelope["data"].get("push").is_none());

    let status = git(dir.path(), &["status", "--short"]);
    assert_success(&status, "git status");
    assert!(status.stdout.is_empty());
}

#[test]
fn ship_json_stops_when_metadata_staging_fails() {
    let dir = repository();
    let head_before = git(dir.path(), &["rev-parse", "HEAD"]);
    assert_success(&head_before, "git rev-parse");

    let config = dir.path().join(".crab.toml");
    let mut config_bytes = std::fs::read(&config).expect("read config");
    config_bytes.extend_from_slice(b"\n# pending metadata\n");
    std::fs::write(config, config_bytes).expect("update config");
    std::fs::write(dir.path().join(".git/index.lock"), b"locked").expect("lock index");

    let output = crab(
        dir.path(),
        &[
            "ship",
            "--message",
            "must not commit",
            "--no-push",
            "--json",
        ],
    );

    assert!(!output.status.success());
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).expect("ship JSON");
    assert_eq!(envelope["schema"], "ship");
    assert!(envelope.get("data").is_none());
    assert!(envelope.get("error").is_some());

    let head_after = git(dir.path(), &["rev-parse", "HEAD"]);
    assert_success(&head_after, "git rev-parse after failure");
    assert_eq!(head_after.stdout, head_before.stdout);
}
