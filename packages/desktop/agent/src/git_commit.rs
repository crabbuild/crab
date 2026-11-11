//! Commit composer backend.
//!
//! Implements the `git_commit` RPC verb for the desktop agent. Builds
//! a `git commit` invocation from structured params — amend, sign-off,
//! GPG/SSH signing, author-date override, and co-author trailers — and
//! returns the resulting commit SHA + subject.
//!
//! Signing delegates entirely to git's own config (`user.signingkey` +
//! GPG/SSH agent). The app never holds private keys.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::{AgentError, Result};

/// Parameters for the `git_commit` verb.
#[derive(Debug, Deserialize)]
pub struct GitCommitParams {
    pub repo_id: String,
    /// Commit message subject (+ optional body). Must not be empty.
    pub message: String,
    /// Commit current working contents of only these literal paths.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// When true, amend the current HEAD commit.
    #[serde(default)]
    pub amend: Option<bool>,
    /// When true, add `Signed-off-by` trailer (`--signoff`).
    #[serde(default)]
    pub sign_off: Option<bool>,
    /// When true, GPG/SSH-sign the commit (`-S`).
    #[serde(default)]
    pub sign: Option<bool>,
    /// Author date handling:
    /// - `"keep"` on amend: preserve original date (git default on amend)
    /// - `"now"`: pass `--date=now`
    /// - any other string: treated as ISO 8601, passed as `--date=<value>`
    /// - `None`: don't pass `--date`
    #[serde(default)]
    pub author_date: Option<String>,
    /// Co-author trailers appended to the commit message body.
    /// Each entry should be in `Name <email>` format.
    #[serde(default)]
    pub co_authored_by: Option<Vec<String>>,
}

/// Execute `git commit` with the given params and return the new
/// commit's SHA and subject.
pub async fn git_commit(work_dir: &Path, params: &GitCommitParams) -> Result<Value> {
    // Validate: message must not be empty.
    if params.message.trim().is_empty() {
        return Err(AgentError::Other(
            "commit message must not be empty".to_owned(),
        ));
    }

    let amend = params.amend.unwrap_or(false);
    let sign_off = params.sign_off.unwrap_or(false);
    let sign = params.sign.unwrap_or(false);

    // Build the final commit message with co-author trailers.
    let final_message = build_message(&params.message, params.co_authored_by.as_deref());

    // Build args.
    let mut args: Vec<String> = vec!["commit".into()];

    if amend {
        args.push("--amend".into());
    }
    if sign_off {
        args.push("--signoff".into());
    }
    if sign {
        args.push("-S".into());
    }

    // Handle author_date.
    if let Some(ref date_val) = params.author_date {
        match date_val.as_str() {
            "keep" => {
                // On amend, git preserves the original date by default.
                // Nothing to pass.
            }
            "now" => {
                args.push("--date=now".into());
            }
            other => {
                args.push(format!("--date={other}"));
            }
        }
    }

    args.push("-m".into());
    args.push(final_message);

    if let Some(paths) = &params.paths {
        if paths.is_empty() {
            return Err(AgentError::Other(
                "Select at least one file to commit".into(),
            ));
        }
        for path in paths {
            crab_sdk::GitPath::new(path.as_bytes().to_vec())?;
        }
        // Git owns partial-commit locking, hooks and index reconciliation.
        // Other staged paths stay staged for the user's next commit.
        args.extend(["--only".into(), "--".into()]);
        args.extend(paths.iter().cloned());
    }

    debug!(args = ?args, "git commit");

    let mut command = crate::shellout::git_command()?;
    command
        .arg("--literal-pathspecs")
        .args(&args)
        .current_dir(work_dir);
    let out = command.output().await?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr,
        });
    }

    // Parse the SHA from git's output. git prints something like:
    //   [main abc1234] subject line
    // or on amend:
    //   [main abc1234] subject line
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (sha, subject) = parse_commit_output(&stdout);

    Ok(json!({
        "sha": sha,
        "subject": subject,
        "amend": amend,
    }))
}

/// Build the final commit message, appending co-author trailers if
/// provided.
fn build_message(message: &str, co_authors: Option<&[String]>) -> String {
    let co_authors = match co_authors {
        Some(list) if !list.is_empty() => list,
        _ => return message.to_owned(),
    };

    let mut result = message.to_owned();

    // Ensure there's a blank line separating the body from trailers.
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result.push('\n');

    for author in co_authors {
        result.push_str(&format!("Co-authored-by: {author}\n"));
    }

    result
}

/// Parse git commit output to extract SHA and subject.
/// Git prints: `[branch sha_prefix] subject`
fn parse_commit_output(output: &str) -> (String, String) {
    // Look for the pattern `[<ref> <sha>] <subject>` in the first line.
    let first_line = output.lines().next().unwrap_or("");

    if let Some(bracket_end) = first_line.find(']') {
        let inside = &first_line[1..bracket_end];
        let subject = first_line[bracket_end + 1..].trim().to_owned();

        // Inside brackets: "branch sha" or "branch (root-commit) sha"
        let sha = inside.split_whitespace().last().unwrap_or("").to_owned();

        (sha, subject)
    } else {
        // Fallback: read HEAD to get the SHA.
        (String::new(), first_line.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Stdio;
    use tempfile::TempDir;

    fn sh(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("git command spawn")
    }

    fn git_init_repo() -> (TempDir, PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().to_path_buf();
        let out = sh(&root, &["init", "--initial-branch=main"]);
        assert!(out.status.success(), "git init: {:?}", out);
        let _ = sh(&root, &["config", "user.email", "test@example.com"]);
        let _ = sh(&root, &["config", "user.name", "Test User"]);
        let _ = sh(&root, &["config", "core.autocrlf", "false"]);
        (td, root)
    }

    fn write_file(dir: &std::path::Path, rel: &str, contents: &str) {
        let abs = dir.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).expect("mkdirs");
        }
        std::fs::write(&abs, contents).expect("write file");
    }

    fn stage_file(root: &std::path::Path, rel: &str) {
        let out = sh(root, &["add", rel]);
        assert!(out.status.success(), "git add: {:?}", out);
    }

    fn get_head_sha(root: &std::path::Path) -> String {
        let out = sh(root, &["rev-parse", "HEAD"]);
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn get_head_message(root: &std::path::Path) -> String {
        let out = sh(root, &["log", "-1", "--format=%B"]);
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn build_message_without_coauthors() {
        let msg = build_message("fix: something", None);
        assert_eq!(msg, "fix: something");
    }

    #[test]
    fn build_message_with_empty_coauthors() {
        let msg = build_message("fix: something", Some(&[]));
        assert_eq!(msg, "fix: something");
    }

    #[test]
    fn build_message_with_coauthors() {
        let authors = vec![
            "Alice <alice@example.com>".to_owned(),
            "Bob <bob@example.com>".to_owned(),
        ];
        let msg = build_message("feat: add feature", Some(&authors));
        assert!(msg.contains("Co-authored-by: Alice <alice@example.com>"));
        assert!(msg.contains("Co-authored-by: Bob <bob@example.com>"));
        // Ensure blank line separates subject from trailers.
        assert!(msg.contains("\n\nCo-authored-by:"));
    }

    #[test]
    fn parse_commit_output_normal() {
        let output = "[main abc1234] fix: something\n";
        let (sha, subject) = parse_commit_output(output);
        assert_eq!(sha, "abc1234");
        assert_eq!(subject, "fix: something");
    }

    #[test]
    fn parse_commit_output_root_commit() {
        let output = "[main (root-commit) def5678] initial commit\n";
        let (sha, subject) = parse_commit_output(output);
        assert_eq!(sha, "def5678");
        assert_eq!(subject, "initial commit");
    }

    #[tokio::test]
    async fn selected_commit_uses_literal_working_paths_and_keeps_other_staged_changes() {
        let (_td, root) = git_init_repo();
        for path in ["a[1].txt", "a1.txt", "other.txt"] {
            write_file(&root, path, "original\n");
            stage_file(&root, path);
        }
        assert!(sh(&root, &["commit", "-m", "initial"]).status.success());
        for path in ["a[1].txt", "a1.txt", "other.txt"] {
            write_file(&root, path, "staged\n");
            stage_file(&root, path);
        }
        let siblings = sh(&root, &["ls-files", "--stage", "--", "a1.txt", "other.txt"]).stdout;
        write_file(&root, "a[1].txt", "current working bytes\n");
        let params = GitCommitParams {
            repo_id: "test".into(),
            message: "selected change".into(),
            paths: Some(vec!["a[1].txt".into()]),
            amend: None,
            sign_off: None,
            sign: None,
            author_date: None,
            co_authored_by: None,
        };
        git_commit(&root, &params).await.expect("selected commit");
        assert_eq!(
            (
                sh(
                    &root,
                    &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]
                )
                .stdout,
                sh(&root, &["show", "HEAD:a[1].txt"]).stdout,
                sh(&root, &["ls-files", "--stage", "--", "a1.txt", "other.txt"]).stdout,
            ),
            (
                b"a[1].txt\n".to_vec(),
                b"current working bytes\n".to_vec(),
                siblings
            ),
        );
    }

    #[tokio::test]
    async fn empty_selection_cannot_commit_the_index() {
        let (_td, root) = git_init_repo();
        write_file(&root, "other.txt", "staged\n");
        stage_file(&root, "other.txt");
        let index = std::fs::read(root.join(".git/index")).expect("index");
        let params = GitCommitParams {
            repo_id: "test".into(),
            message: "empty selection".into(),
            paths: Some(Vec::new()),
            amend: None,
            sign_off: None,
            sign: None,
            author_date: None,
            co_authored_by: None,
        };
        let error = git_commit(&root, &params)
            .await
            .expect_err("empty selection");
        assert!(matches!(error, AgentError::Other(_)));
        assert_eq!(
            std::fs::read(root.join(".git/index")).expect("index"),
            index
        );
    }

    #[tokio::test]
    async fn commit_with_staged_file_succeeds() {
        let (_td, root) = git_init_repo();
        write_file(&root, "hello.txt", "hello world\n");
        stage_file(&root, "hello.txt");

        let params = GitCommitParams {
            repo_id: "test".into(),
            paths: None,
            message: "feat: add hello".into(),
            amend: None,
            sign_off: None,
            sign: None,
            author_date: None,
            co_authored_by: None,
        };

        let result = git_commit(&root, &params).await.expect("commit succeeds");
        let sha = result["sha"].as_str().unwrap_or("");
        assert!(!sha.is_empty(), "SHA should be non-empty");

        // Verify HEAD matches the returned SHA prefix.
        let head = get_head_sha(&root);
        assert!(
            head.starts_with(sha),
            "HEAD {head} should start with returned sha {sha}"
        );
    }

    #[tokio::test]
    async fn commit_amend_changes_head() {
        let (_td, root) = git_init_repo();
        write_file(&root, "a.txt", "first\n");
        stage_file(&root, "a.txt");
        let _ = sh(&root, &["commit", "-m", "initial"]);
        let original_head = get_head_sha(&root);

        // Amend with a new message.
        let params = GitCommitParams {
            repo_id: "test".into(),
            paths: None,
            message: "amended message".into(),
            amend: Some(true),
            sign_off: None,
            sign: None,
            author_date: None,
            co_authored_by: None,
        };

        let result = git_commit(&root, &params).await.expect("amend succeeds");
        let new_head = get_head_sha(&root);

        assert_ne!(original_head, new_head, "HEAD should change after amend");
        assert_eq!(result["amend"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn commit_with_coauthors_appends_trailers() {
        let (_td, root) = git_init_repo();
        write_file(&root, "b.txt", "content\n");
        stage_file(&root, "b.txt");

        let params = GitCommitParams {
            repo_id: "test".into(),
            paths: None,
            message: "feat: collaborative work".into(),
            amend: None,
            sign_off: None,
            sign: None,
            author_date: None,
            co_authored_by: Some(vec![
                "Alice <alice@example.com>".into(),
                "Bob <bob@example.com>".into(),
            ]),
        };

        let _result = git_commit(&root, &params).await.expect("commit succeeds");
        let msg = get_head_message(&root);
        assert!(
            msg.contains("Co-authored-by: Alice <alice@example.com>"),
            "message should contain Alice trailer: {msg}"
        );
        assert!(
            msg.contains("Co-authored-by: Bob <bob@example.com>"),
            "message should contain Bob trailer: {msg}"
        );
    }

    #[tokio::test]
    async fn commit_empty_message_fails() {
        let (_td, root) = git_init_repo();
        write_file(&root, "c.txt", "data\n");
        stage_file(&root, "c.txt");

        let params = GitCommitParams {
            repo_id: "test".into(),
            paths: None,
            message: "   ".into(), // whitespace-only
            amend: None,
            sign_off: None,
            sign: None,
            author_date: None,
            co_authored_by: None,
        };

        let err = git_commit(&root, &params).await.unwrap_err();
        assert!(
            matches!(err, AgentError::Other(ref msg) if msg.contains("empty")),
            "expected empty-message error, got: {err}"
        );
    }

    #[tokio::test]
    async fn commit_nothing_staged_fails() {
        let (_td, root) = git_init_repo();
        // Create a file but don't stage it.
        write_file(&root, "d.txt", "unstaged\n");

        let params = GitCommitParams {
            repo_id: "test".into(),
            paths: None,
            message: "should fail".into(),
            amend: None,
            sign_off: None,
            sign: None,
            author_date: None,
            co_authored_by: None,
        };

        let err = git_commit(&root, &params).await.unwrap_err();
        assert!(
            matches!(err, AgentError::Cli { .. }),
            "expected CLI error for nothing staged, got: {err}"
        );
    }
}
