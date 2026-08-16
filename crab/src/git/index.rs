use std::path::Path;
use std::process::Command;

use crate::core::error::{CrabError, Result};

/// Stage repository metadata paths and propagate Git failures.
pub(crate) fn stage_paths(repo_root: &Path, paths: &[&str]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("add")
        .arg("--")
        .args(paths)
        .current_dir(repo_root)
        .output()?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else if !stdout.trim().is_empty() {
        stdout.trim()
    } else {
        "no diagnostic output"
    };
    Err(CrabError::Internal(format!(
        "git add repository metadata failed: {detail}"
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn metadata_staging_propagates_git_index_failure() {
        let dir = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        if !init.success() {
            eprintln!("SKIP: git init failed");
            return;
        }

        std::fs::write(dir.path().join(".crab.toml"), "[remote]\n").unwrap();
        std::fs::write(dir.path().join(".git/index.lock"), "locked").unwrap();

        let error = stage_paths(dir.path(), &[".crab.toml"])
            .expect_err("Git index lock must reject metadata staging");

        assert!(
            error
                .to_string()
                .contains("git add repository metadata failed")
        );
    }
}
