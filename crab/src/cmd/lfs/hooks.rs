//! Git LFS compatibility hook commands.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::core::error::{CrabError, Result};
use crate::lfs::lock::LockManager;

use super::store_setup::{git_user_identity, resolve_lfs_remote_sync};

#[derive(Debug, Default, PartialEq, Eq)]
struct PathAttrs {
    filter: Option<String>,
    lockable: Option<String>,
}

/// Run `crab lfs post-checkout`.
pub fn run_post_checkout(args: &[String]) -> Result<ExitCode> {
    if !valid_hook_args("post-checkout", args, 3) {
        return Ok(ExitCode::FAILURE);
    }
    run_lockable_hook()
}

/// Run `crab lfs post-commit`.
pub fn run_post_commit(args: &[String]) -> Result<ExitCode> {
    if !valid_hook_args("post-commit", args, 0) {
        return Ok(ExitCode::FAILURE);
    }
    run_lockable_hook()
}

/// Run `crab lfs post-merge`.
pub fn run_post_merge(args: &[String]) -> Result<ExitCode> {
    if !valid_hook_args("post-merge", args, 1) {
        return Ok(ExitCode::FAILURE);
    }
    run_lockable_hook()
}

fn valid_hook_args(command: &str, args: &[String], expected: usize) -> bool {
    if args.len() == expected {
        return true;
    }

    eprintln!("This should be run through Git's {command} hook.");
    eprintln!("Run `crab lfs update` to install or repair the hook.");
    false
}

fn run_lockable_hook() -> Result<ExitCode> {
    let repo_root = std::env::current_dir()?;
    let tracked_files = git_ls_files(&repo_root)?;
    if tracked_files.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    let lockable_files = git_lockable_lfs_files(&repo_root, &tracked_files)?;
    if lockable_files.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    let writable_paths = current_user_lock_paths();
    apply_lockable_permissions(&repo_root, &lockable_files, &writable_paths)?;
    Ok(ExitCode::SUCCESS)
}

fn git_ls_files(repo_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Configuration {
            key: "git ls-files".to_owned(),
            origin: format!("failed to run git ls-files: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CrabError::Configuration {
            key: "git ls-files".to_owned(),
            origin: if stderr.is_empty() {
                "git ls-files failed".to_owned()
            } else {
                stderr
            },
        });
    }

    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).to_string())
        .collect())
}

fn git_lockable_lfs_files(repo_root: &Path, files: &[String]) -> Result<Vec<String>> {
    let mut child = Command::new("git")
        .args(["check-attr", "-z", "--stdin", "filter", "lockable"])
        .current_dir(repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CrabError::Configuration {
            key: "git check-attr".to_owned(),
            origin: format!("failed to run git check-attr: {e}"),
        })?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| CrabError::Internal("failed to open git check-attr stdin".to_owned()))?;
        for file in files {
            stdin.write_all(file.as_bytes())?;
            stdin.write_all(&[0])?;
        }
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CrabError::Configuration {
            key: "git check-attr".to_owned(),
            origin: if stderr.is_empty() {
                "git check-attr failed".to_owned()
            } else {
                stderr
            },
        });
    }

    Ok(lockable_lfs_files_from_check_attr(&output.stdout))
}

fn lockable_lfs_files_from_check_attr(raw: &[u8]) -> Vec<String> {
    let mut attrs_by_path: HashMap<String, PathAttrs> = HashMap::new();
    let mut fields = raw.split(|b| *b == 0).filter(|field| !field.is_empty());

    while let (Some(path), Some(attr), Some(value)) = (fields.next(), fields.next(), fields.next())
    {
        let path = String::from_utf8_lossy(path).to_string();
        let attr = String::from_utf8_lossy(attr);
        let value = String::from_utf8_lossy(value).to_string();
        let attrs = attrs_by_path.entry(path).or_default();
        match attr.as_ref() {
            "filter" => attrs.filter = Some(value),
            "lockable" => attrs.lockable = Some(value),
            _ => {}
        }
    }

    attrs_by_path
        .into_iter()
        .filter_map(|(path, attrs)| {
            (attrs.filter.as_deref() == Some("lfs") && attrs.lockable.as_deref() == Some("set"))
                .then_some(path)
        })
        .collect()
}

fn current_user_lock_paths() -> HashSet<String> {
    let Ok(owner) = git_user_identity() else {
        return HashSet::new();
    };
    let Ok(ctx) = resolve_lfs_remote_sync() else {
        return HashSet::new();
    };

    let lock_store = crate::storage::Store::from_storage(ctx.store.store().clone());
    let mgr = LockManager::lfs(lock_store, &ctx.prefix);
    match super::block_on_runtime(async move {
        let records = mgr.list().await?;
        Ok(records
            .into_iter()
            .filter(|record| record.owner == owner)
            .map(|record| record.path)
            .collect())
    }) {
        Ok(paths) => paths,
        Err(error) => {
            tracing::warn!(%error, "failed to read LFS locks while updating lockable files");
            HashSet::new()
        }
    }
}

fn apply_lockable_permissions(
    repo_root: &Path,
    lockable_files: &[String],
    writable_paths: &HashSet<String>,
) -> Result<usize> {
    let mut updated = 0;
    for file in lockable_files {
        let path = repo_root.join(file);
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }

        set_file_writable(&path, writable_paths.contains(file))?;
        updated += 1;
    }
    Ok(updated)
}

#[cfg(unix)]
fn set_file_writable(path: &Path, writable: bool) -> Result<()> {
    let metadata = fs::metadata(path)?;
    let mut permissions = metadata.permissions();
    let current = permissions.mode();
    let desired = if writable {
        current | 0o200
    } else {
        current & !0o222
    };

    if current != desired {
        permissions.set_mode(desired);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_file_writable(path: &Path, writable: bool) -> Result<()> {
    let metadata = fs::metadata(path)?;
    let mut permissions = metadata.permissions();
    let readonly = !writable;
    if permissions.readonly() != readonly {
        permissions.set_readonly(readonly);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn check_attr_record(path: &str, filter: &str, lockable: &str) -> Vec<u8> {
        [
            path.as_bytes(),
            b"\0filter\0",
            filter.as_bytes(),
            b"\0",
            path.as_bytes(),
            b"\0lockable\0",
            lockable.as_bytes(),
            b"\0",
        ]
        .concat()
    }

    #[test]
    fn check_attr_parser_finds_lockable_lfs_files() {
        let mut raw = Vec::new();
        raw.extend(check_attr_record("a.psd", "lfs", "set"));
        raw.extend(check_attr_record("b.bin", "lfs", "unspecified"));
        raw.extend(check_attr_record("c.dat", "crab", "set"));

        let mut files = lockable_lfs_files_from_check_attr(&raw);
        files.sort();

        assert_eq!(files, vec!["a.psd"]);
    }

    #[test]
    fn apply_permissions_makes_unlocked_lockable_files_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.psd");
        fs::write(&file, b"content").unwrap();

        let count =
            apply_lockable_permissions(dir.path(), &["a.psd".to_owned()], &HashSet::new()).unwrap();

        assert_eq!(count, 1);
        assert!(fs::metadata(file).unwrap().permissions().readonly());
    }

    #[test]
    fn apply_permissions_keeps_owned_locks_writable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.psd");
        fs::write(&file, b"content").unwrap();
        set_file_writable(&file, false).unwrap();

        let count = apply_lockable_permissions(
            dir.path(),
            &["a.psd".to_owned()],
            &HashSet::from(["a.psd".to_owned()]),
        )
        .unwrap();

        assert_eq!(count, 1);
        assert!(!fs::metadata(file).unwrap().permissions().readonly());
    }
}
