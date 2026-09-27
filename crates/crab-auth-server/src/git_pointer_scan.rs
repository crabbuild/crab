//! Complete reachable Git pointer scanning for server-side verification.

use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};

use crab_git::{
    PointerKind, classify,
    lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE},
};
use crab_types::pointer::Pointer;

use crate::error::{AuthServerError, Result};

#[derive(Debug, Default)]
pub(crate) struct ReachablePointerScan {
    pub crab_pointers: Vec<Pointer>,
    pub lfs_pointers: Vec<LfsPointer>,
}

pub(crate) fn scan_reachable_pointers(git_dir: &Path) -> Result<ReachablePointerScan> {
    scan_reachable_pointers_from_refs(git_dir, &[])
}

pub(crate) fn scan_reachable_pointers_from_refs(
    git_dir: &Path,
    refs: &[(String, String)],
) -> Result<ReachablePointerScan> {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-list", "--objects"]);
    if refs.is_empty() {
        command.arg("--all");
    } else {
        command.args(refs.iter().map(|(_, oid)| oid));
    }
    let output = command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        return Err(AuthServerError::Internal(format!(
            "git pointer scan failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let output = String::from_utf8(output.stdout).map_err(|error| {
        AuthServerError::Internal(format!("git pointer scan output is not UTF-8: {error}"))
    })?;
    let mut seen_objects = HashSet::new();
    let mut seen_lfs_oids = HashSet::new();
    let mut scan = ReachablePointerScan::default();

    for line in output.lines() {
        let Some(oid) = line.split_whitespace().next() else {
            continue;
        };
        if !seen_objects.insert(oid.to_owned()) {
            continue;
        }
        if run_git_capture(git_dir, ["cat-file", "-t", oid])?.trim() != "blob" {
            continue;
        }
        let size = run_git_capture(git_dir, ["cat-file", "-s", oid])?
            .trim()
            .parse::<usize>()
            .map_err(|error| {
                AuthServerError::Internal(format!("git cat-file returned bad size: {error}"))
            })?;
        if size > MAX_LFS_POINTER_SIZE {
            continue;
        }

        let bytes = run_git_capture_bytes(git_dir, ["cat-file", "blob", oid])?;
        match classify(&bytes) {
            PointerKind::Crab(pointer) => scan.crab_pointers.push(pointer),
            PointerKind::Lfs(pointer) if pointer.size > 0 => {
                if seen_lfs_oids.insert(pointer.oid) {
                    scan.lfs_pointers.push(pointer);
                }
            }
            PointerKind::Lfs(_) | PointerKind::NotAPointer => {}
        }
    }
    Ok(scan)
}

fn run_git_capture<const N: usize>(git_dir: &Path, args: [&str; N]) -> Result<String> {
    String::from_utf8(run_git_capture_bytes(git_dir, args)?)
        .map_err(|error| AuthServerError::Internal(format!("git output is not UTF-8: {error}")))
}

fn run_git_capture_bytes<const N: usize>(git_dir: &Path, args: [&str; N]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(AuthServerError::Internal(format!(
        "git pointer scan failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}
