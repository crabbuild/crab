//! Post-fetch connectivity checker.
//!
//! Walks the commit graph from ref tips and verifies that every referenced
//! commit, tree, and blob exists in the local `.git/objects/` directory
//! (loose or packed). Uses Git's streaming object enumeration and the issue
//! taxonomy from [`crate::cmd::fsck`].

use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::core::error::{CrabError, Result};

/// Outcome of a connectivity check.
#[derive(Debug, Clone)]
pub struct ConnectivityResult {
    /// Total objects checked (commits + trees + blobs).
    pub objects_checked: u64,
    /// Hex OIDs of objects referenced but missing from the local ODB.
    pub missing: Vec<String>,
    /// `true` if the walk completed; `false` if cancelled early.
    pub complete: bool,
}

/// Check that all objects reachable from `ref_tips` exist locally.
///
/// Opens the git ODB at `git_dir/objects` and streams the commit → tree → blob
/// graph from `git rev-list`. Missing objects are collected but do not cause an
/// error — the caller decides how to surface them.
///
/// # Cancellation
///
/// The token is checked between objects. If cancelled, the function returns
/// an incomplete result (`complete = false`) with whatever was checked so far.
///
/// # Errors
///
/// Returns [`CrabError::Io`] if the objects directory is missing, or
/// [`CrabError::Internal`] when Git cannot enumerate the requested graph.
pub async fn check_connectivity(
    git_dir: &Path,
    ref_tips: &[String],
    cancel: &CancellationToken,
) -> Result<ConnectivityResult> {
    check_connectivity_with_frontier(git_dir, ref_tips, &[], cancel).await
}

/// Check connectivity for objects reachable from `ref_tips`, excluding a
/// trusted remote frontier.
///
/// `frontier_ref_tips` must contain commit tips whose reachable objects are
/// already durable on the remote. When present, the checker enumerates only
/// `ref_tips ^frontier_ref_tips` and validates those local objects; when
/// empty, the same streaming checker walks the full graph.
pub async fn check_connectivity_with_frontier(
    git_dir: &Path,
    ref_tips: &[String],
    frontier_ref_tips: &[String],
    cancel: &CancellationToken,
) -> Result<ConnectivityResult> {
    let git_dir = git_dir.to_path_buf();
    let tips: Vec<String> = ref_tips.to_vec();
    let frontier: Vec<String> = frontier_ref_tips.to_vec();
    let token = cancel.clone();

    tokio::task::spawn_blocking(move || check_connectivity_sync(&git_dir, &tips, &frontier, &token))
        .await
        .map_err(|e| CrabError::Internal(format!("connectivity check join error: {e}")))?
}

/// Synchronous implementation of the connectivity walk.
fn check_connectivity_sync(
    git_dir: &Path,
    ref_tips: &[String],
    frontier_ref_tips: &[String],
    cancel: &CancellationToken,
) -> Result<ConnectivityResult> {
    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return Err(CrabError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("git objects directory not found: {}", objects_dir.display()),
        )));
    }

    let mut tip_shas = Vec::<String>::with_capacity(ref_tips.len());
    for sha in ref_tips {
        match ObjectId::from_hex(sha.as_bytes()) {
            Ok(oid) => {
                tip_shas.push(oid.to_string());
            }
            Err(e) => {
                warn!(sha = %sha, error = %e, "skipping ref tip with invalid SHA");
            }
        }
    }

    if tip_shas.is_empty() {
        debug!("no valid ref tips for connectivity check");
        return Ok(ConnectivityResult {
            objects_checked: 0,
            missing: Vec::new(),
            complete: true,
        });
    }

    let frontier = normalize_frontier_ref_tips(frontier_ref_tips);
    check_streaming_connectivity_sync(git_dir, &tip_shas, &frontier, cancel)
}

fn normalize_frontier_ref_tips(frontier_ref_tips: &[String]) -> Vec<String> {
    let mut tips = BTreeSet::new();
    for sha in frontier_ref_tips {
        match ObjectId::from_hex(sha.as_bytes()) {
            Ok(oid) => {
                tips.insert(oid.to_string());
            }
            Err(e) => {
                warn!(sha = %sha, error = %e, "skipping frontier tip with invalid SHA");
            }
        }
    }
    tips.into_iter().collect()
}

fn check_streaming_connectivity_sync(
    git_dir: &Path,
    tip_shas: &[String],
    frontier_ref_tips: &[String],
    cancel: &CancellationToken,
) -> Result<ConnectivityResult> {
    if cancel.is_cancelled() {
        return Ok(ConnectivityResult {
            objects_checked: 0,
            missing: Vec::new(),
            complete: false,
        });
    }

    let rev_stderr = tempfile::NamedTempFile::new()?;
    let revision_input = build_revision_input(tip_shas, frontier_ref_tips);
    let mut rev_list = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-list", "--stdin", "--objects", "--missing=print"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(rev_stderr.reopen()?))
        .spawn()
        .map_err(CrabError::Io)?;

    let mut rev_stdin = rev_list
        .stdin
        .take()
        .ok_or_else(|| CrabError::Internal("rev-list stdin not available".to_owned()))?;
    let writer = std::thread::spawn(move || {
        rev_stdin.write_all(revision_input.as_bytes())?;
        rev_stdin.flush()
    });

    let rev_stdout = rev_list
        .stdout
        .take()
        .ok_or_else(|| CrabError::Internal("rev-list stdout not available".to_owned()))?;
    let mut reader = std::io::BufReader::new(rev_stdout);
    let mut line = String::new();
    let mut missing = Vec::<String>::new();
    let mut objects_checked = 0u64;

    loop {
        line.clear();
        let read = match reader.read_line(&mut line) {
            Ok(read) => read,
            Err(e) => {
                let _ = rev_list.kill();
                let _ = rev_list.wait();
                let _ = writer.join();
                return Err(CrabError::Io(e));
            }
        };
        if read == 0 {
            break;
        }

        if cancel.is_cancelled() {
            let _ = rev_list.kill();
            let _ = rev_list.wait();
            let _ = writer.join();
            return Ok(ConnectivityResult {
                objects_checked,
                missing,
                complete: false,
            });
        }

        let object = match parse_rev_list_object_line(&line) {
            Ok(Some(object)) => object,
            Ok(None) => continue,
            Err(e) => {
                let _ = rev_list.kill();
                let _ = rev_list.wait();
                let _ = writer.join();
                return Err(e);
            }
        };
        // Git's object traversal marks emitted objects as SEEN, so each
        // reachable object is produced once. Keeping a second process-local
        // set here would make Crab memory scale with repository size.
        objects_checked += 1;

        match object {
            RevListObject::Present => {}
            RevListObject::Missing(oid) => missing.push(oid.to_string()),
        }
    }

    let writer_result = writer
        .join()
        .map_err(|_| CrabError::Internal("rev-list stdin writer panicked".to_owned()))?;
    let rev_status = rev_list.wait().map_err(CrabError::Io)?;
    if !rev_status.success() {
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {}",
            read_tempfile_stderr(&rev_stderr)
        )));
    }
    writer_result.map_err(CrabError::Io)?;

    debug!(
        objects_checked,
        missing_count = missing.len(),
        frontier_tips = frontier_ref_tips.len(),
        "streaming connectivity check complete"
    );

    Ok(ConnectivityResult {
        objects_checked,
        missing,
        complete: true,
    })
}

enum RevListObject {
    Present,
    Missing(ObjectId),
}

fn parse_rev_list_object_line(line: &str) -> Result<Option<RevListObject>> {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let (missing, oid_text) = match trimmed.strip_prefix('?') {
        Some(rest) => (
            true,
            rest.split_ascii_whitespace().next().unwrap_or_default(),
        ),
        None => (
            false,
            trimmed.split_ascii_whitespace().next().unwrap_or_default(),
        ),
    };
    if oid_text.is_empty() {
        return Ok(None);
    }

    let oid = ObjectId::from_hex(oid_text.as_bytes()).map_err(|e| {
        CrabError::Internal(format!(
            "git rev-list returned invalid object id '{oid_text}': {e}"
        ))
    })?;

    Ok(Some(if missing {
        RevListObject::Missing(oid)
    } else {
        RevListObject::Present
    }))
}

fn build_revision_input(tip_shas: &[String], frontier_ref_tips: &[String]) -> String {
    let mut input = String::new();
    for sha in frontier_ref_tips {
        input.push('^');
        input.push_str(sha);
        input.push('\n');
    }
    for sha in tip_shas {
        input.push_str(sha);
        input.push('\n');
    }
    input
}

fn read_tempfile_stderr(file: &tempfile::NamedTempFile) -> String {
    std::fs::read_to_string(file.path()).unwrap_or_else(|e| format!("<failed to read stderr: {e}>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connectivity_result_defaults() {
        let result = ConnectivityResult {
            objects_checked: 0,
            missing: Vec::new(),
            complete: true,
        };
        assert!(result.complete);
        assert!(result.missing.is_empty());
        assert_eq!(result.objects_checked, 0);
    }

    #[tokio::test]
    async fn check_connectivity_missing_objects_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();

        let tips = vec!["a".repeat(40)];
        let cancel = CancellationToken::new();
        let err = check_connectivity(&git_dir, &tips, &cancel)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn check_connectivity_no_tips_returns_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();

        let cancel = CancellationToken::new();
        let result = check_connectivity(&git_dir, &[], &cancel).await.unwrap();
        assert!(result.complete);
        assert_eq!(result.objects_checked, 0);
        assert!(result.missing.is_empty());
    }

    #[tokio::test]
    async fn check_connectivity_invalid_tips_returns_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();

        let tips = vec!["not-hex".to_string(), "short".to_string()];
        let cancel = CancellationToken::new();
        let result = check_connectivity(&git_dir, &tips, &cancel).await.unwrap();
        assert!(result.complete);
        assert_eq!(result.objects_checked, 0);
    }

    #[tokio::test]
    async fn check_connectivity_cancelled_returns_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();

        // Even with a valid-looking tip, cancellation should return early.
        let tips = vec!["a".repeat(40)];
        let result = check_connectivity(&git_dir, &tips, &cancel).await.unwrap();
        assert!(!result.complete);
    }

    #[tokio::test]
    async fn check_connectivity_on_real_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();

        let status = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(repo_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let Ok(s) = status else {
            eprintln!("skipping test: git not available");
            return;
        };
        if !s.success() {
            eprintln!("skipping test: git init failed");
            return;
        }

        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo_dir)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(repo_dir)
            .status();

        std::fs::write(repo_dir.join("hello.txt"), b"hello world\n").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "hello.txt"])
            .current_dir(repo_dir)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        let head_sha = String::from_utf8(output.stdout).unwrap().trim().to_string();

        if head_sha.len() != 40 {
            eprintln!("skipping test: could not get HEAD sha");
            return;
        }

        let git_dir = repo_dir.join(".git");
        let tips = vec![head_sha];
        let cancel = CancellationToken::new();
        let result = check_connectivity(&git_dir, &tips, &cancel).await.unwrap();

        assert!(result.complete);
        assert!(result.missing.is_empty(), "expected no missing objects");
        // 1 commit + 1 tree + 1 blob = 3 objects minimum
        assert!(
            result.objects_checked >= 3,
            "expected at least 3 objects, got {}",
            result.objects_checked
        );
    }

    #[tokio::test]
    async fn check_connectivity_skips_submodule_gitlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();
        let gitlink_oid = "52b7efa603f1b809167b528b8bbaa467e36fdc02";

        macro_rules! git {
            ($($arg:expr),+ $(,)?) => {
                std::process::Command::new("git")
                    .args([$($arg),+])
                    .current_dir(repo_dir)
            };
        }

        let Ok(s) = git!("init", "--initial-branch=main")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        else {
            eprintln!("skipping test: git not available");
            return;
        };
        if !s.success() {
            eprintln!("skipping test: git init failed");
            return;
        }

        let _ = git!("config", "user.email", "test@test.com").status();
        let _ = git!("config", "user.name", "Test").status();
        let _ = git!(
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            gitlink_oid,
            "vendor/lib"
        )
        .status();
        let _ = git!("commit", "-m", "add submodule")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let output = git!("rev-parse", "HEAD").output().unwrap();
        let head_sha = String::from_utf8(output.stdout).unwrap().trim().to_string();
        if head_sha.len() != 40 {
            eprintln!("skipping test: could not get HEAD sha");
            return;
        }

        let git_dir = repo_dir.join(".git");
        let cancel = CancellationToken::new();
        let result = check_connectivity(&git_dir, &[head_sha], &cancel)
            .await
            .unwrap();

        assert!(result.complete);
        assert!(
            result.missing.is_empty(),
            "gitlinks must not be required in the superproject ODB: {:?}",
            result.missing
        );
    }

    // --- Sub-task 2.6: connectivity tests ---

    /// Build a mini git repo with one commit, then deliberately
    /// remove the commit's tree object from `.git/objects/`. The
    /// connectivity walker should complete (`complete=true`) and
    /// surface the missing tree OID in its `missing` list.
    ///
    /// This is the "missing parent" analogue in reachability
    /// terms — a commit whose subgraph is incomplete because a
    /// referenced object is gone from the ODB.
    #[tokio::test]
    async fn connectivity_rejects_pack_with_missing_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();

        macro_rules! git {
            ($($arg:expr),+ $(,)?) => {
                std::process::Command::new("git")
                    .args([$($arg),+])
                    .current_dir(repo_dir)
            };
        }

        let Ok(init) = git!("init", "--initial-branch=main")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        else {
            eprintln!("skipping: git not available");
            return;
        };
        if !init.success() {
            eprintln!("skipping: git init failed");
            return;
        }

        let _ = git!("config", "user.email", "test@test.com").status();
        let _ = git!("config", "user.name", "Test").status();

        std::fs::write(repo_dir.join("hello.txt"), b"hello\n").unwrap();
        let _ = git!("add", "hello.txt").status();
        let _ = git!("commit", "-m", "initial")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        let out = git!("rev-parse", "HEAD").output().unwrap();
        let head_sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
        if head_sha.len() != 40 {
            eprintln!("skipping: could not read HEAD sha");
            return;
        }

        // Find the commit's tree SHA to remove.
        let tree_out = git!("rev-parse", "HEAD^{tree}").output().unwrap();
        let tree_sha = String::from_utf8(tree_out.stdout)
            .unwrap()
            .trim()
            .to_string();
        if tree_sha.len() != 40 {
            eprintln!("skipping: could not read HEAD tree sha");
            return;
        }

        // Delete the loose tree object to simulate a missing object
        // the packer forgot to include.
        let git_dir = repo_dir.join(".git");
        let tree_path = git_dir
            .join("objects")
            .join(&tree_sha[..2])
            .join(&tree_sha[2..]);
        if tree_path.exists() {
            std::fs::remove_file(&tree_path).unwrap();
        } else {
            // Tree may have been packed — no cheap way to force-extract
            // and delete from here. Skip the test rather than produce a
            // false positive.
            eprintln!("skipping: tree object is packed, not loose");
            return;
        }

        let tips = vec![head_sha];
        let cancel = CancellationToken::new();
        let result = check_connectivity(&git_dir, &tips, &cancel).await.unwrap();

        assert!(result.complete, "walk should complete");
        assert!(
            !result.missing.is_empty(),
            "expected at least one missing object, got none"
        );
        assert!(
            result.missing.iter().any(|m| m == &tree_sha),
            "expected removed tree {tree_sha} in missing list, got {:?}",
            result.missing
        );
    }

    /// Walk over the full edit set (updates + creates together).
    ///
    /// Build a repo with two commits on two branches. Walk from both
    /// tips and confirm both branches contribute to the object
    /// count — the walker covers "updates + creates uniformly" per
    /// the spec's sub-task 2.1.
    #[tokio::test]
    async fn connectivity_on_full_edit_set_covers_updates_and_creates() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();

        macro_rules! git {
            ($($arg:expr),+ $(,)?) => {
                std::process::Command::new("git")
                    .args([$($arg),+])
                    .current_dir(repo_dir)
            };
        }

        let Ok(init) = git!("init", "--initial-branch=main")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        else {
            eprintln!("skipping: git not available");
            return;
        };
        if !init.success() {
            eprintln!("skipping: git init failed");
            return;
        }

        let _ = git!("config", "user.email", "test@test.com").status();
        let _ = git!("config", "user.name", "Test").status();

        // Commit on main: update tip.
        std::fs::write(repo_dir.join("main.txt"), b"main\n").unwrap();
        let _ = git!("add", "main.txt").status();
        let _ = git!("commit", "-m", "main commit")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let main_out = git!("rev-parse", "HEAD").output().unwrap();
        let main_sha = String::from_utf8(main_out.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Branch `feature` off main, add another file: create tip.
        let _ = git!("checkout", "-b", "feature")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::fs::write(repo_dir.join("feature.txt"), b"feature\n").unwrap();
        let _ = git!("add", "feature.txt").status();
        let _ = git!("commit", "-m", "feature commit")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let feature_out = git!("rev-parse", "HEAD").output().unwrap();
        let feature_sha = String::from_utf8(feature_out.stdout)
            .unwrap()
            .trim()
            .to_string();

        if main_sha.len() != 40 || feature_sha.len() != 40 {
            eprintln!("skipping: could not read both branch tips");
            return;
        }

        let git_dir = repo_dir.join(".git");

        // Walk from just main: expect 2 objects at minimum (commit + tree).
        // Blobs may or may not be walked depending on whether the file
        // object is present; either way at least commit + tree are
        // reachable.
        let cancel = CancellationToken::new();
        let main_only = check_connectivity(&git_dir, &[main_sha.clone()], &cancel)
            .await
            .unwrap();
        assert!(main_only.complete);
        assert!(main_only.missing.is_empty(), "main must be complete");

        // Walk from both tips: more objects, same complete+empty-missing.
        // The feature tip is a child of main so the walker visits main's
        // commit + tree via feature's parent chain plus feature's own
        // commit + tree — total ≥ main_only + 1 new commit.
        let both = check_connectivity(&git_dir, &[main_sha, feature_sha], &cancel)
            .await
            .unwrap();
        assert!(both.complete);
        assert!(both.missing.is_empty(), "both branches must be complete");
        assert!(
            both.objects_checked > main_only.objects_checked,
            "full edit set walk should touch more objects than main-only: main_only={}, both={}",
            main_only.objects_checked,
            both.objects_checked,
        );
    }

    fn init_frontier_test_repo(repo_dir: &Path) -> bool {
        let Ok(init) = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(repo_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        else {
            eprintln!("skipping: git not available");
            return false;
        };
        if !init.success() {
            eprintln!("skipping: git init failed");
            return false;
        }

        let _ = git_status(repo_dir, ["config", "user.email", "test@test.com"]);
        let _ = git_status(repo_dir, ["config", "user.name", "Test"]);
        true
    }

    fn git_status<const N: usize>(
        repo_dir: &Path,
        args: [&str; N],
    ) -> std::io::Result<std::process::ExitStatus> {
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    }

    fn git_output<const N: usize>(repo_dir: &Path, args: [&str; N]) -> Option<String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo_dir)
            .output()
            .ok()?;
        Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
    }

    fn commit_all(repo_dir: &Path, message: &str) {
        let _ = git_status(repo_dir, ["add", "."]);
        let _ = git_status(repo_dir, ["commit", "-m", message]);
    }

    #[tokio::test]
    async fn connectivity_reports_missing_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();
        if !init_frontier_test_repo(repo_dir) {
            return;
        }

        std::fs::write(repo_dir.join("payload.txt"), b"payload\n").unwrap();
        commit_all(repo_dir, "payload");
        let Some(head_sha) = git_output(repo_dir, ["rev-parse", "HEAD"]) else {
            eprintln!("skipping: could not read head sha");
            return;
        };
        let Some(blob_sha) = git_output(repo_dir, ["rev-parse", "HEAD:payload.txt"]) else {
            eprintln!("skipping: could not read blob sha");
            return;
        };

        let git_dir = repo_dir.join(".git");
        let blob_path = git_dir
            .join("objects")
            .join(&blob_sha[..2])
            .join(&blob_sha[2..]);
        std::fs::remove_file(blob_path).unwrap();

        let result = check_connectivity(&git_dir, &[head_sha], &CancellationToken::new())
            .await
            .unwrap();

        assert!(result.complete);
        assert_eq!(result.missing, vec![blob_sha]);
    }

    #[tokio::test]
    async fn check_connectivity_walks_annotated_tag_tip() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();
        if !init_frontier_test_repo(repo_dir) {
            return;
        }

        std::fs::write(repo_dir.join("release.txt"), b"release\n").unwrap();
        commit_all(repo_dir, "release");
        let _ = git_status(repo_dir, ["tag", "-a", "v1.0", "-m", "release"]);
        let Some(tag_sha) = git_output(repo_dir, ["rev-parse", "refs/tags/v1.0"]) else {
            eprintln!("skipping: could not read tag sha");
            return;
        };
        if tag_sha.len() != 40 {
            eprintln!("skipping: could not read annotated tag sha");
            return;
        }

        let cancel = CancellationToken::new();
        let result = check_connectivity(&repo_dir.join(".git"), &[tag_sha], &cancel)
            .await
            .unwrap();

        assert!(result.complete);
        assert!(result.missing.is_empty());
        assert!(
            result.objects_checked >= 4,
            "annotated tag tip must check tag, commit, tree, and blob objects; checked {}",
            result.objects_checked
        );
    }

    #[tokio::test]
    async fn frontier_connectivity_checks_only_new_reachable_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();
        if !init_frontier_test_repo(repo_dir) {
            return;
        }

        for i in 0..48 {
            std::fs::write(
                repo_dir.join(format!("file-{i}.txt")),
                format!("base {i}\n"),
            )
            .unwrap();
        }
        commit_all(repo_dir, "base");
        let Some(base_sha) = git_output(repo_dir, ["rev-parse", "HEAD"]) else {
            eprintln!("skipping: could not read base sha");
            return;
        };

        std::fs::write(repo_dir.join("file-0.txt"), b"changed\n").unwrap();
        let _ = git_status(repo_dir, ["add", "file-0.txt"]);
        let _ = git_status(repo_dir, ["commit", "-m", "incremental"]);
        let Some(head_sha) = git_output(repo_dir, ["rev-parse", "HEAD"]) else {
            eprintln!("skipping: could not read head sha");
            return;
        };

        if base_sha.len() != 40 || head_sha.len() != 40 {
            eprintln!("skipping: could not read commit shas");
            return;
        }

        let git_dir = repo_dir.join(".git");
        let cancel = CancellationToken::new();
        let full = check_connectivity(&git_dir, std::slice::from_ref(&head_sha), &cancel)
            .await
            .unwrap();
        let frontier =
            check_connectivity_with_frontier(&git_dir, &[head_sha], &[base_sha], &cancel)
                .await
                .unwrap();

        assert!(full.complete);
        assert!(frontier.complete);
        assert!(full.missing.is_empty());
        assert!(frontier.missing.is_empty());
        assert!(
            frontier.objects_checked < full.objects_checked,
            "frontier should check fewer objects than full walk: frontier={}, full={}",
            frontier.objects_checked,
            full.objects_checked
        );
    }

    #[tokio::test]
    async fn frontier_connectivity_reports_missing_new_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();
        if !init_frontier_test_repo(repo_dir) {
            return;
        }

        std::fs::write(repo_dir.join("base.txt"), b"base\n").unwrap();
        commit_all(repo_dir, "base");
        let Some(base_sha) = git_output(repo_dir, ["rev-parse", "HEAD"]) else {
            eprintln!("skipping: could not read base sha");
            return;
        };

        std::fs::write(repo_dir.join("next.txt"), b"next\n").unwrap();
        commit_all(repo_dir, "next");
        let Some(head_sha) = git_output(repo_dir, ["rev-parse", "HEAD"]) else {
            eprintln!("skipping: could not read head sha");
            return;
        };
        let Some(tree_sha) = git_output(repo_dir, ["rev-parse", "HEAD^{tree}"]) else {
            eprintln!("skipping: could not read head tree sha");
            return;
        };

        if base_sha.len() != 40 || head_sha.len() != 40 || tree_sha.len() != 40 {
            eprintln!("skipping: could not read commit/tree shas");
            return;
        }

        let git_dir = repo_dir.join(".git");
        let tree_path = git_dir
            .join("objects")
            .join(&tree_sha[..2])
            .join(&tree_sha[2..]);
        if tree_path.exists() {
            std::fs::remove_file(&tree_path).unwrap();
        } else {
            eprintln!("skipping: tree object is packed, not loose");
            return;
        }

        let cancel = CancellationToken::new();
        let result = check_connectivity_with_frontier(&git_dir, &[head_sha], &[base_sha], &cancel)
            .await
            .unwrap();

        assert!(result.complete);
        assert!(
            result.missing.iter().any(|oid| oid == &tree_sha),
            "expected removed tree {tree_sha} in missing list, got {:?}",
            result.missing
        );
    }
}
