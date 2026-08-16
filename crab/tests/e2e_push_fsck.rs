//! Regression tests for push-side object validation during pack indexing.
//!
//! Two behaviours are exercised against real Git packs:
//!
//!   * A hand-crafted malformed tree is rejected before installation.
//!   * A hand-crafted malformed commit (missing required `author`
//!     header) is rejected the same way.
//!
//! The tests write loose objects via `git hash-object --literally -w`
//! which bypasses git's own fsck, giving us real malformed bodies
//! addressed by their canonical SHA-1.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crab::git::pack::install_pack_file_locally;

/// Ephemeral `.git/` directory with a single seed commit. Each test
/// owns its own repo so there's no cross-test ODB contamination.
struct Repo {
    _tmp: tempfile::TempDir,
    work: PathBuf,
}

impl Repo {
    fn new() -> Option<Self> {
        let tmp = tempfile::tempdir().ok()?;
        let work = tmp.path().to_path_buf();
        if !run_git_silent(&work, &["init", "--initial-branch=main"]) {
            eprintln!("skipping: git init failed (git not available?)");
            return None;
        }
        run_git_silent(&work, &["config", "user.email", "test@example.com"]);
        run_git_silent(&work, &["config", "user.name", "Test"]);
        std::fs::write(work.join("seed.txt"), b"seed\n").ok()?;
        run_git_silent(&work, &["add", "seed.txt"]);
        if !run_git_silent(&work, &["commit", "-m", "seed"]) {
            return None;
        }
        Some(Self { _tmp: tmp, work })
    }

    /// Hash bytes as an object of the given kind and write it to the
    /// repo's loose-object store. `--literally` skips git's own
    /// canonical-form fsck so malformed bodies can be materialized.
    /// Returns the hex OID of the written object.
    fn write_literal(&self, kind: &str, body: &[u8]) -> Option<String> {
        let mut child = Command::new("git")
            .current_dir(&self.work)
            .args(["hash-object", "-w", "--stdin", "--literally", "-t", kind])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;

        {
            use std::io::Write;
            let mut stdin = child.stdin.take()?;
            stdin.write_all(body).ok()?;
        }
        let out = child.wait_with_output().ok()?;
        if !out.status.success() {
            eprintln!(
                "git hash-object --literally failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        let sha = String::from_utf8(out.stdout).ok()?.trim().to_owned();
        if sha.len() != 40 {
            return None;
        }
        Some(sha)
    }

    fn pack_commit(&self, commit_sha: &str) -> Option<Vec<u8>> {
        let mut child = Command::new("git")
            .current_dir(&self.work)
            .args(["pack-objects", "--stdout", "--revs"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        {
            use std::io::Write;
            child
                .stdin
                .take()?
                .write_all(format!("{commit_sha}\n").as_bytes())
                .ok()?;
        }
        let output = child.wait_with_output().ok()?;
        output.status.success().then_some(output.stdout)
    }
}

async fn install_pack(pack_bytes: &[u8]) -> std::result::Result<(), crab::core::error::CrabError> {
    let temp = tempfile::tempdir()?;
    let pack_dir = temp.path().join("objects/pack");
    std::fs::create_dir_all(&pack_dir)?;
    let source = temp.path().join("source.pack");
    std::fs::write(&source, pack_bytes)?;
    install_pack_file_locally(&pack_dir, &source, "push-fsck", 0, true).await?;
    Ok(())
}

fn run_git_silent(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build a tree body that fails canonical parsing.
///
/// The mode prefix must be pure octal digits up to the space
/// delimiter; injecting a non-octal byte makes
/// `gix_object::TreeRef::from_bytes` return `decode::Error`.
fn malformed_tree_body() -> Vec<u8> {
    // Format per entry: `<mode> <name>\0<20-byte-oid>`.
    // Put a non-octal byte ('9') in the mode so the parser bails.
    let zero_oid = [0u8; 20];
    let mut body = Vec::new();
    // `199999` — all octal? `9` is not octal.
    body.extend_from_slice(b"199999 a\0");
    body.extend_from_slice(&zero_oid);
    body
}

/// Build a commit body with no `author` header. `tree` + `committer`
/// are present and well-formed; the missing author line alone is
/// enough for `CommitRef::from_bytes` to reject the body.
fn commit_body_missing_author(tree_sha: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"tree ");
    body.extend_from_slice(tree_sha.as_bytes());
    body.push(b'\n');
    body.extend_from_slice(b"committer Test <test@example.com> 1700000000 +0000\n\nmessage\n");
    body
}

/// A hand-crafted malformed tree is rejected while indexing the pack.
#[tokio::test]
async fn malformed_tree_rejected() {
    let Some(repo) = Repo::new() else { return };

    // Write a malformed tree directly.
    let Some(bad_tree_sha) = repo.write_literal("tree", &malformed_tree_body()) else {
        eprintln!("skipping: could not stage malformed tree");
        return;
    };

    // Wrap it in a commit so the connectivity walker visits it via
    // the normal commit → tree edge. `git commit-tree` runs with
    // `GIT_AUTHOR_*` / `GIT_COMMITTER_*` env, but `hash-object -t
    // commit --literally` is simpler and keeps all-in-one test
    // control.
    let commit_body = format!(
        "tree {bad_tree_sha}\nauthor Test <test@example.com> 1700000000 +0000\ncommitter Test <test@example.com> 1700000000 +0000\n\nfsck regression\n",
    );
    let Some(commit_sha) = repo.write_literal("commit", commit_body.as_bytes()) else {
        eprintln!("skipping: could not stage wrapper commit");
        return;
    };

    let Some(pack_bytes) = repo.pack_commit(&commit_sha) else {
        eprintln!("skipping: could not pack malformed tree");
        return;
    };

    install_pack(&pack_bytes)
        .await
        .expect_err("fsck must reject malformed tree");
}

/// A commit body missing the required `author` header is rejected
/// while indexing its pack.
#[tokio::test]
async fn malformed_commit_missing_author_rejected() {
    let Some(repo) = Repo::new() else { return };

    // Use the empty tree as the "valid-enough" tree so fsck only
    // fires on the commit-level violation. The well-known empty-
    // tree SHA is universal.
    let empty_tree_sha = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    // Make sure the empty tree exists locally — `git hash-object -w`
    // with empty stdin gives us it.
    let _ = repo
        .write_literal("tree", b"")
        .unwrap_or_else(|| empty_tree_sha.into());

    let Some(bad_commit_sha) =
        repo.write_literal("commit", &commit_body_missing_author(empty_tree_sha))
    else {
        eprintln!("skipping: could not stage malformed commit");
        return;
    };

    let Some(pack_bytes) = repo.pack_commit(&bad_commit_sha) else {
        eprintln!("skipping: could not pack malformed commit");
        return;
    };

    install_pack(&pack_bytes)
        .await
        .expect_err("fsck must reject malformed commit");
}
