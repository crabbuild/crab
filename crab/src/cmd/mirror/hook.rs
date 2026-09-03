//! Local mirror pre-push hook inspection.

use std::path::{Path, PathBuf};

use super::types::{MirrorHookState, MirrorHookStatus};

/// Inspect the effective pre-push hook for a local source working tree.
pub fn mirror_hook_status(root: &Path) -> MirrorHookStatus {
    if !root.is_dir() {
        return MirrorHookStatus {
            state: MirrorHookState::NotApplicable,
            path: None,
            detail: Some("source is not a local working tree".to_owned()),
        };
    }
    let output = std::process::Command::new("git")
        .args([
            "rev-parse",
            "--is-bare-repository",
            "--git-path",
            "hooks/pre-push",
        ])
        .current_dir(root)
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return MirrorHookStatus {
                state: MirrorHookState::NotApplicable,
                path: None,
                detail: Some(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
            };
        }
        Err(error) => {
            return MirrorHookStatus {
                state: MirrorHookState::Unverifiable,
                path: None,
                detail: Some(error.to_string()),
            };
        }
    };
    let lines = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if lines.first().is_some_and(|line| line == "true") {
        return MirrorHookStatus {
            state: MirrorHookState::NotApplicable,
            path: None,
            detail: Some("source is a bare repository".to_owned()),
        };
    }
    let Some(hook_value) = lines.get(1) else {
        return MirrorHookStatus {
            state: MirrorHookState::Unverifiable,
            path: None,
            detail: Some("git did not return the pre-push hook path".to_owned()),
        };
    };
    let hook_path = {
        let path = PathBuf::from(hook_value);
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    };
    let content = match std::fs::read_to_string(&hook_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return MirrorHookStatus {
                state: MirrorHookState::Missing,
                path: Some(hook_path.display().to_string()),
                detail: Some("run `crab init --mirror <remote>` to install it".to_owned()),
            };
        }
        Err(error) => {
            return MirrorHookStatus {
                state: MirrorHookState::Unverifiable,
                path: Some(hook_path.display().to_string()),
                detail: Some(error.to_string()),
            };
        }
    };
    let executable = hook_is_executable(&hook_path);
    if content == crate::cmd::install::MIRROR_PRE_PUSH_HOOK
        || content == crate::cmd::install::MIRROR_LFS_PRE_PUSH_HOOK
    {
        if executable {
            return MirrorHookStatus {
                state: MirrorHookState::Installed,
                path: Some(hook_path.display().to_string()),
                detail: None,
            };
        }
        return MirrorHookStatus {
            state: MirrorHookState::Missing,
            path: Some(hook_path.display().to_string()),
            detail: Some("mirror pre-push hook is not executable".to_owned()),
        };
    }
    if content.contains(crate::cmd::install::OBSOLETE_MIRROR_PRE_PUSH_BODY) {
        return MirrorHookStatus {
            state: MirrorHookState::Missing,
            path: Some(hook_path.display().to_string()),
            detail: Some(
                "mirror pre-push hook is obsolete; rerun `crab init` to update it".to_owned(),
            ),
        };
    }
    MirrorHookStatus {
        state: MirrorHookState::Missing,
        path: Some(hook_path.display().to_string()),
        detail: Some("mirror pre-push hook block is absent".to_owned()),
    }
}

#[cfg(unix)]
fn hook_is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn hook_is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    #[test]
    fn working_tree_without_hook_is_missing() {
        let dir = init_repo();
        let status = mirror_hook_status(dir.path());
        assert_eq!(status.state, MirrorHookState::Missing);
        assert!(status.path.is_some());
    }

    #[test]
    fn non_repository_is_not_applicable() {
        let dir = tempfile::tempdir().unwrap();
        let status = mirror_hook_status(dir.path());
        assert_eq!(status.state, MirrorHookState::NotApplicable);
    }

    #[cfg(unix)]
    #[test]
    fn installed_executable_hook_is_detected() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = init_repo();
        let hook = dir.path().join(".git/hooks/pre-push");
        std::fs::write(&hook, crate::cmd::install::MIRROR_PRE_PUSH_HOOK).unwrap();
        let mut permissions = hook.metadata().unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();

        let status = mirror_hook_status(dir.path());
        assert_eq!(status.state, MirrorHookState::Installed);
    }

    #[cfg(unix)]
    #[test]
    fn obsolete_executable_hook_is_not_reported_as_installed() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = init_repo();
        let hook = dir.path().join(".git/hooks/pre-push");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\n{}",
                crate::cmd::install::OBSOLETE_MIRROR_PRE_PUSH_BODY
            ),
        )
        .unwrap();
        let mut permissions = hook.metadata().unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();

        let status = mirror_hook_status(dir.path());
        assert_eq!(status.state, MirrorHookState::Missing);
        assert!(status.detail.unwrap().contains("obsolete"));
    }
}
