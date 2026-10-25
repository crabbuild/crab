use std::process::Command;

fn git_forwarder() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let forwarder = directory.path().join(name);
    std::fs::hard_link(env!("CARGO_BIN_EXE_crab"), &forwarder)
        .or_else(|_| std::fs::copy(env!("CARGO_BIN_EXE_crab"), &forwarder).map(drop))
        .unwrap();
    (directory, forwarder, git)
}

#[test]
fn nested_git_preserves_the_selected_installation_and_arguments() {
    let (directory, forwarder, git) = git_forwarder();
    for args in [
        vec!["--version"],
        vec![
            "-c",
            "test.value=space ' quote & 海",
            "config",
            "--get",
            "test.value",
        ],
    ] {
        let expected = Command::new(&git)
            .args(&args)
            .current_dir(directory.path())
            .output()
            .unwrap();
        let actual = Command::new(&forwarder)
            .env(crab_remote::local::GIT_EXECUTABLE_ENV, &git)
            .args(&args)
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert_eq!(
            (actual.status.code(), actual.stdout, actual.stderr),
            (expected.status.code(), expected.stdout, expected.stderr)
        );
    }
}

#[test]
fn nested_git_without_a_selected_target_fails_before_cli_dispatch() {
    let (directory, forwarder, _) = git_forwarder();
    let output = Command::new(forwarder)
        .env_remove(crab_remote::local::GIT_EXECUTABLE_ENV)
        .arg("--version")
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success()
            && output.stdout.is_empty()
            && String::from_utf8_lossy(&output.stderr)
                .contains("selected Git executable is missing")
    );
}

fn crab_json(args: &[&str]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args(args)
        .output()
        .expect("run crab");
    assert!(
        output.status.success(),
        "crab {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse crab JSON output")
}

#[test]
fn sdk_capabilities_report_the_executable_version() {
    let capabilities = crab_json(&["sdk-capabilities", "--json"]);
    let version = crab_json(&["version", "--json"]);

    assert_eq!(
        capabilities["crab_version"],
        version["data"]["crab_version"]
    );
}

#[test]
fn sdk_capabilities_report_canonical_staging_format() {
    let capabilities = crab_json(&["sdk-capabilities", "--json"]);

    assert_eq!(capabilities["staging_format_version"], 1);
}
