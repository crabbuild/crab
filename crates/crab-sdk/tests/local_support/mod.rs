#![cfg(feature = "local")]

use std::path::{Path, PathBuf};
use std::process::Command;

use crab_sdk::Client;
use crab_sdk::local::{ExecutionPolicy as LocalExecutionPolicy, Options, Tools as LocalTools};
use crab_sdk::storage::DirectStoreOptions;

pub fn run(git: &Path, directory: &Path, args: &[&str]) -> String {
    let output = Command::new(git)
        .current_dir(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

pub fn git_path() -> PathBuf {
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let candidate = directory.join(if cfg!(windows) { "git.exe" } else { "git" });
        if candidate.is_file() {
            return candidate.canonicalize().unwrap();
        }
    }
    panic!("Git is required for the local SDK test")
}

#[cfg(unix)]
pub fn fake_crab(directory: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join("crab fixture");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = sdk-capabilities ]; then\n  printf '%s\\n' '{}'\n  exit 0\nfi\nif [ \"$1\" = hydrate ] && [ -f \"$0.enable-hydrate\" ]; then\n  shift\n  printf '%s\\n' \"$@\" > \"$0.hydrate\"\n  exit 0\nfi\nif [ \"$1\" = fetch ] && [ -f \"$0.enable-fetch\" ]; then\n  shift\n  printf '%s\\n' \"$@\" > \"$0.fetch\"\n  exit 0\nfi\nexit 64\n",
            crab_remote::local::CURRENT_CAPABILITIES_JSON
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(windows)]
pub fn fake_crab(directory: &Path) -> PathBuf {
    let source = std::env::var_os("CRAB_SDK_TEST_CRAB_BIN")
        .map(PathBuf::from)
        .expect("CRAB_SDK_TEST_CRAB_BIN must name the compatible Crab test executable");
    let path = directory.join("crab fixture's path.exe");
    std::fs::copy(source, &path).unwrap();
    path
}

pub fn client(root: &Path, scratch: &Path) -> Client {
    client_with_policy(root, scratch, LocalExecutionPolicy::Untrusted)
}

pub fn client_with_policy(root: &Path, scratch: &Path, policy: LocalExecutionPolicy) -> Client {
    Client::builder()
        .direct_store(DirectStoreOptions::filesystem(root).unwrap())
        .local(
            Options::new(LocalTools::new(git_path(), fake_crab(scratch)).unwrap())
                .with_execution_policy(policy),
        )
        .build()
        .unwrap()
}

pub fn commit(git: &Path, repository: &Path, name: &str, content: &str) -> String {
    std::fs::write(repository.join(name), content).unwrap();
    run(git, repository, &["add", "--", name]);
    run(
        git,
        repository,
        &[
            "-c",
            "user.name=SDK test",
            "-c",
            "user.email=sdk@example.invalid",
            "commit",
            "-m",
            content,
        ],
    );
    run(git, repository, &["rev-parse", "HEAD"])
}
