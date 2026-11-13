use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{AgentError, Result};

#[derive(Deserialize)]
pub(crate) struct Params {
    worktree_path: PathBuf,
    base_ref: String,
}

pub(crate) async fn compare(p: Params) -> Result<Value> {
    let cwd = &p.worktree_path;
    let root = git(cwd, &["rev-parse", "--show-toplevel"], None).await?;
    if tokio::fs::canonicalize(root.trim_end_matches(['\r', '\n'])).await?
        != tokio::fs::canonicalize(cwd).await?
    {
        return Err(AgentError::Other(
            "Saved workspace is not its Git worktree root".into(),
        ));
    }
    let baseline = format!("{}^{{tree}}", p.base_ref);
    let tree = git(
        cwd,
        &["rev-parse", "--verify", "--end-of-options", &baseline],
        None,
    )
    .await
    .map_err(|source| AgentError::Managed {
        kind: "comparison_baseline".into(),
        message: format!("Cannot resolve file comparison baseline: {}", p.base_ref),
        remediation: "Restore the saved baseline before reloading changes".into(),
        source: Some(Box::new(source)),
    })?;
    let directory = tempfile::tempdir()?;
    let index = directory.path().join("index");
    let original = git(cwd, &["rev-parse", "--git-path", "index"], None).await?;
    let original = cwd.join(original.trim_end_matches(['\r', '\n']));
    if let Err(error) = tokio::fs::copy(original, &index).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    // Intent entries include new and saved-untracked paths without staging
    // content. Every Git write targets this private index, never the user's.
    git(
        cwd,
        &["add", "--intent-to-add", "--all", "--", "."],
        Some(&index),
    )
    .await?;
    let output = git(
        cwd,
        &[
            "diff",
            "--patch-with-raw",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--find-renames",
            "--submodule=short",
            "--output-indicator-new=+",
            "--output-indicator-old=-",
            "--output-indicator-context= ",
            tree.trim(),
            "--",
        ],
        Some(&index),
    )
    .await?;
    let (diff, name_status) = split_output(&output)?;
    // v2.0.0-alpha.1 shipped this response shape. Derive its path metadata
    // from the same Git invocation as the patches so concurrent edits cannot
    // pair a filename with a different file's content.
    let head_ref = match git(cwd, &["rev-parse", "--verify", "HEAD^{commit}"], None).await {
        Ok(head) => head.trim().to_owned(),
        Err(error) => {
            let symbolic = git(cwd, &["symbolic-ref", "HEAD"], None).await?;
            let refs = git(
                cwd,
                &["for-each-ref", "--format=%(refname)", symbolic.trim()],
                None,
            )
            .await?;
            if refs.lines().any(|reference| reference == symbolic.trim()) {
                return Err(error);
            }
            // The tagged wire format uses "unknown" for an unborn HEAD.
            "unknown".into()
        }
    };
    Ok(json!({ "diff": diff, "name_status": name_status, "head_ref": head_ref }))
}

async fn git(cwd: &Path, args: &[&str], index: Option<&Path>) -> Result<String> {
    let mut command = crate::shellout::git_command()?;
    if let Some(index) = index {
        // Intent entries need stat refresh for raw paths to agree with patches.
        // This private index belongs to the comparison; refreshing the user's
        // real index remains disabled by the shared command owner.
        command
            .args(["-c", "diff.autoRefreshIndex=true"])
            .env("GIT_INDEX_FILE", index);
    }
    command
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Other("Git stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AgentError::Other("Git stderr unavailable".into()))?;
    let capture = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::try_join!(
            child.wait(),
            read_limited(stdout, 50 * 1024 * 1024),
            read_limited(stderr, 1024 * 1024)
        )
    })
    .await;
    let (status, stdout, stderr) = match capture {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            child.kill().await?;
            return Err(error.into());
        }
        Err(error) => {
            child.kill().await?;
            return Err(AgentError::Managed {
                kind: "comparison_timeout".into(),
                message: "Git file comparison timed out".into(),
                remediation: "Repair the repository or its required filters, then reload changes"
                    .into(),
                source: Some(Box::new(error)),
            });
        }
    };
    if !status.success() {
        return Err(AgentError::Cli {
            status: status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        });
    }
    Ok(String::from_utf8(stdout)?)
}

async fn read_limited(stream: impl AsyncRead + Unpin, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    stream.take(limit + 1).read_to_end(&mut output).await?;
    if output.len() as u64 > limit {
        return Err(std::io::Error::other(
            "Git file comparison exceeded its output limit",
        ));
    }
    Ok(output)
}

fn split_output(mut output: &str) -> Result<(&str, String)> {
    let mut names = String::new();
    while output.starts_with(':') {
        let (header, rest) = field(output)?;
        let code = header.rsplit(' ').next().ok_or_else(incomplete)?;
        names.push_str(code);
        output = rest;
        let count = if code.starts_with(['R', 'C']) { 2 } else { 1 };
        for _ in 0..count {
            let (path, rest) = field(output)?;
            names.push('\t');
            names.push_str(&quote_path(path));
            output = rest;
        }
        names.push('\n');
    }
    Ok((output.strip_prefix('\0').unwrap_or(output), names))
}

fn field(output: &str) -> Result<(&str, &str)> {
    output.split_once('\0').ok_or_else(incomplete)
}

fn incomplete() -> AgentError {
    AgentError::Other("Git returned an incomplete file comparison".into())
}

fn quote_path(path: &str) -> String {
    if path
        .bytes()
        .all(|byte| (b' '..=b'~').contains(&byte) && !matches!(byte, b'"' | b'\\'))
    {
        return path.into();
    }
    let mut quoted = String::from("\"");
    for byte in path.bytes() {
        match byte {
            b'"' => quoted.push_str("\\\""),
            b'\\' => quoted.push_str("\\\\"),
            b' '..=b'~' => quoted.push(char::from(byte)),
            _ => quoted.push_str(&format!("\\{byte:03o}")),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(root: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn git_init_repo() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        assert!(
            sh(&root, &["init", "--initial-branch=main"])
                .status
                .success()
        );
        assert!(
            sh(&root, &["config", "user.email", "fixture@example.invalid"])
                .status
                .success()
        );
        assert!(
            sh(&root, &["config", "user.name", "Fixture"])
                .status
                .success()
        );
        (directory, root)
    }

    #[tokio::test]
    async fn task_diff_includes_mixed_changes_and_preserves_the_index() {
        let (_td, root) = git_init_repo();
        std::fs::write(root.join("tracked.txt"), "before\n").unwrap();
        assert!(sh(&root, &["add", "."]).status.success());
        assert!(sh(&root, &["commit", "-m", "Baseline"]).status.success());
        let base_ref = String::from_utf8(sh(&root, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        let index = std::fs::read(root.join(".git/index")).unwrap();
        std::fs::write(root.join("tracked.txt"), "after\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let result = compare(Params {
            worktree_path: root.clone(),
            base_ref,
        })
        .await
        .unwrap();
        assert_eq!(
            (
                result["name_status"].as_str().unwrap(),
                std::fs::read(root.join(".git/index")).unwrap()
            ),
            ("A\tnew.txt\nM\ttracked.txt\n", index)
        );
    }

    #[tokio::test]
    async fn task_diff_rejects_a_missing_baseline() {
        let (_td, root) = git_init_repo();
        assert!(
            sh(&root, &["commit", "--allow-empty", "-m", "Baseline"])
                .status
                .success()
        );
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let result = compare(Params {
            worktree_path: root.clone(),
            base_ref: "missing-baseline".into(),
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn unborn_preview_leaves_the_real_index_absent() {
        let (_td, root) = git_init_repo();
        let tree = String::from_utf8(sh(&root, &["mktree"]).stdout).unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let result = compare(Params {
            worktree_path: root.clone(),
            base_ref: tree.trim().into(),
        })
        .await
        .unwrap();
        assert_eq!(
            (
                result["name_status"].as_str(),
                result["head_ref"].as_str(),
                root.join(".git/index").exists()
            ),
            (Some("A\tnew.txt\n"), Some("unknown"), false)
        );
    }

    #[tokio::test]
    async fn untracked_baseline_bytes_are_not_reported_as_deleted() {
        let (_td, root) = git_init_repo();
        std::fs::write(root.join("saved.txt"), "user bytes\n").unwrap();
        assert!(sh(&root, &["add", "."]).status.success());
        assert!(sh(&root, &["commit", "-m", "Baseline"]).status.success());
        assert!(sh(&root, &["rm", "--cached", "saved.txt"]).status.success());
        let result = compare(Params {
            worktree_path: root,
            base_ref: "HEAD".into(),
        })
        .await
        .unwrap();
        assert_eq!(
            (&result["diff"], &result["name_status"]),
            (&json!(""), &json!(""))
        );
    }

    #[tokio::test]
    async fn split_index_and_staged_ignored_additions_survive_preview() {
        let (_td, root) = git_init_repo();
        assert!(
            sh(&root, &["commit", "--allow-empty", "-m", "Baseline"])
                .status
                .success()
        );
        std::fs::write(root.join("staged.txt"), "staged bytes\n").unwrap();
        assert!(sh(&root, &["add", "staged.txt"]).status.success());
        assert!(
            sh(&root, &["update-index", "--split-index"])
                .status
                .success()
        );
        std::fs::write(root.join(".gitignore"), "staged.txt\n").unwrap();
        let index = std::fs::read(root.join(".git/index")).unwrap();
        let staged = sh(&root, &["diff", "--cached"]).stdout;
        let result = compare(Params {
            worktree_path: root.clone(),
            base_ref: "HEAD".into(),
        })
        .await
        .unwrap();
        assert_eq!(
            (
                result["name_status"].as_str(),
                std::fs::read(root.join(".git/index")).unwrap(),
                sh(&root, &["diff", "--cached"]).stdout
            ),
            (Some("A\t.gitignore\nA\tstaged.txt\n"), index, staged)
        );
    }

    #[tokio::test]
    async fn required_filter_errors_do_not_become_partial_comparisons() {
        let (_td, root) = git_init_repo();
        assert!(
            sh(&root, &["commit", "--allow-empty", "-m", "Baseline"])
                .status
                .success()
        );
        std::fs::write(root.join(".gitattributes"), "*.bin filter=fixture\n").unwrap();
        std::fs::write(root.join("payload.bin"), "bytes\n").unwrap();
        assert!(
            sh(&root, &["config", "filter.fixture.clean", "exit 17"])
                .status
                .success()
        );
        assert!(
            sh(&root, &["config", "filter.fixture.required", "true"])
                .status
                .success()
        );
        let index = std::fs::read(root.join(".git/index")).ok();
        let result = compare(Params {
            worktree_path: root.clone(),
            base_ref: "HEAD".into(),
        })
        .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read(root.join(".git/index")).ok(), index);
    }

    #[tokio::test]
    async fn replaced_folders_cannot_read_the_parent_repository() {
        let (_td, root) = git_init_repo();
        assert!(
            sh(&root, &["commit", "--allow-empty", "-m", "Baseline"])
                .status
                .success()
        );
        let replacement = root.join("replaced");
        std::fs::create_dir(&replacement).unwrap();
        assert!(
            compare(Params {
                worktree_path: replacement,
                base_ref: "HEAD".into()
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn quoted_wire_paths_keep_utf8_identity() {
        let (_td, root) = git_init_repo();
        assert!(
            sh(&root, &["commit", "--allow-empty", "-m", "Baseline"])
                .status
                .success()
        );
        std::fs::write(root.join("雪.txt"), "content\n").unwrap();
        let result = compare(Params {
            worktree_path: root,
            base_ref: "HEAD".into(),
        })
        .await
        .unwrap();
        assert_eq!(result["name_status"], "A\t\"\\351\\233\\252.txt\"\n");
    }
}
