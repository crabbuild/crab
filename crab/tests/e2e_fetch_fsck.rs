//! Regression tests for fetch-side pack installation and bounded validation.
//!
//! Publication owns semantic fsck. Fetch installs immutable pack bytes
//! structurally, then verifies that requested tips resolve after every sibling
//! pack is visible.
//!
//! The tests shell out to `git hash-object --literally -w` and `git
//! pack-objects` so we exercise the real Git pack-install path
//! end-to-end. Tests skip cleanly when `git` is not on `$PATH`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crab::git::pack::{
    install_pack_file_locally, rollback_installed_pack, validate_fetched_ref_tips,
};

/// Ephemeral `.git/` directory with a single seed commit. Each test
/// owns its own repo so the loose object store stays isolated.
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

    fn all_commits(&self) -> Option<Vec<String>> {
        let out = Command::new("git")
            .current_dir(&self.work)
            .args(["rev-list", "--all"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8(out.stdout).ok()?;
        Some(text.lines().map(ToOwned::to_owned).collect())
    }

    /// Build a pack containing the given commit OIDs via
    /// `git pack-objects --stdout`. The returned bytes are a real
    /// git pack (PACK header + entries + trailing SHA-1).
    fn pack_commits(&self, commit_shas: &[&str]) -> Option<Vec<u8>> {
        self.pack_commits_with_args(commit_shas, &[])
    }

    fn pack_exact_objects(&self, object_shas: &[&str]) -> Option<Vec<u8>> {
        use std::io::Write;

        let mut child = Command::new("git")
            .current_dir(&self.work)
            .args(["pack-objects", "--stdout"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        {
            let mut stdin = child.stdin.take()?;
            for sha in object_shas {
                stdin.write_all(sha.as_bytes()).ok()?;
                stdin.write_all(b"\n").ok()?;
            }
        }
        let output = child.wait_with_output().ok()?;
        output.status.success().then_some(output.stdout)
    }

    fn pack_commits_with_args(&self, commit_shas: &[&str], extra_args: &[&str]) -> Option<Vec<u8>> {
        use std::io::Write;

        let mut rev_list = String::new();
        for sha in commit_shas {
            rev_list.push_str(sha);
            rev_list.push('\n');
        }

        let mut child = Command::new("git")
            .current_dir(&self.work)
            .args(["pack-objects", "--stdout", "--revs"])
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        {
            let mut stdin = child.stdin.take()?;
            stdin.write_all(rev_list.as_bytes()).ok()?;
        }
        let out = child.wait_with_output().ok()?;
        if !out.status.success() {
            eprintln!(
                "git pack-objects failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        if out.stdout.len() < 32 || &out.stdout[..4] != b"PACK" {
            return None;
        }
        Some(out.stdout)
    }

    fn pack_has_ref_delta_chain(&self, pack_bytes: &[u8]) -> Option<bool> {
        let tmp = tempfile::tempdir().ok()?;
        let pack_path = tmp.path().join("probe.pack");
        let idx_path = tmp.path().join("probe.idx");
        std::fs::write(&pack_path, pack_bytes).ok()?;

        let out = Command::new("git")
            .current_dir(&self.work)
            .arg("index-pack")
            .arg("-o")
            .arg(&idx_path)
            .arg(&pack_path)
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!(
                "git index-pack probe failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }

        let idx = gix_pack::index::File::at(&idx_path, gix_hash::Kind::Sha1).ok()?;
        let pack = gix_pack::data::File::at(&pack_path, gix_hash::Kind::Sha1).ok()?;

        for entry in idx.iter() {
            let pack_entry = pack.entry(entry.pack_offset).ok()?;
            let gix_pack::data::entry::Header::RefDelta { base_id } = pack_entry.header else {
                continue;
            };
            let Some(base_idx) = idx.lookup(base_id.as_ref()) else {
                continue;
            };
            let base_offset = idx.pack_offset_at_index(base_idx);
            let base_entry = pack.entry(base_offset).ok()?;
            if base_entry.header.is_delta() {
                return Some(true);
            }
        }

        Some(false)
    }
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

fn temp_git_pack_dir() -> (tempfile::TempDir, PathBuf) {
    let dest = tempfile::tempdir().expect("tempdir");
    assert!(run_git_silent(dest.path(), &["init", "--bare"]));
    let pack_dir = dest.path().join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir).expect("pack dir");
    (dest, pack_dir)
}

/// Rollback is idempotent: calling it on an already-clean pack
/// directory returns `Ok` without error. This guards the fetch
/// retry loop from double-rollback failure modes.
#[tokio::test]
async fn rollback_installed_pack_is_idempotent() {
    let dest = tempfile::tempdir().expect("tempdir");
    // No files to begin with.
    rollback_installed_pack(dest.path(), "nothing-installed")
        .await
        .expect("rollback on empty pack dir must succeed");

    // And a second call is still fine.
    rollback_installed_pack(dest.path(), "nothing-installed")
        .await
        .expect("second rollback must still succeed");
}

/// Structural installation plus bounded tip validation accepts a well-formed pack.
#[tokio::test]
async fn fetch_accepts_well_formed_pack() {
    let Some(repo) = Repo::new() else { return };

    // The seed commit built by `Repo::new` has a canonical tree
    // and canonical commit, so fsck must accept its pack.
    let head = match Command::new("git")
        .current_dir(&repo.work)
        .args(["rev-parse", "HEAD"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        _ => {
            eprintln!("skipping: could not rev-parse HEAD");
            return;
        }
    };
    let Some(pack_bytes) = repo.pack_commits(&[&head]) else {
        eprintln!("skipping: could not build well-formed pack");
        return;
    };

    let (_dest, pack_dir) = temp_git_pack_dir();
    let canonical_name = "good-pack";

    let source = pack_dir.join("good-source.pack");
    std::fs::write(&source, pack_bytes).expect("write good source pack");
    let installed = install_pack_file_locally(&pack_dir, &source, canonical_name, 0, false)
        .await
        .expect("well-formed install must succeed");
    validate_fetched_ref_tips(
        pack_dir
            .parent()
            .and_then(Path::parent)
            .expect("git directory"),
        &[head],
    )
    .await
    .expect("requested tip must resolve");
    assert!(installed.pack_path.exists());
    assert!(installed.idx_path.exists());
}

/// Fetch installs a requested tip whose tree dependency lives in a sibling pack.
#[tokio::test]
async fn fetch_accepts_cross_pack_gitattributes_dependency() {
    let Some(repo) = Repo::new() else { return };

    let Some(attributes_sha) = repo.write_literal("blob", b"*.txt text\n") else {
        return;
    };
    let attributes_oid = match gix_hash::ObjectId::from_hex(attributes_sha.as_bytes()) {
        Ok(oid) => oid,
        Err(_) => return,
    };
    let mut tree_body = b"100644 .gitattributes\0".to_vec();
    tree_body.extend_from_slice(attributes_oid.as_bytes());
    let Some(tree_sha) = repo.write_literal("tree", &tree_body) else {
        return;
    };
    let commit_body = format!(
        "tree {tree_sha}\nauthor Test <test@example.com> 1700000000 +0000\ncommitter Test <test@example.com> 1700000000 +0000\n\ncross-pack attributes\n"
    );
    let Some(commit_sha) = repo.write_literal("commit", commit_body.as_bytes()) else {
        return;
    };
    let Some(graph_pack) = repo.pack_exact_objects(&[&commit_sha, &tree_sha]) else {
        return;
    };
    let Some(attributes_pack) = repo.pack_exact_objects(&[&attributes_sha]) else {
        return;
    };

    let (_dest, pack_dir) = temp_git_pack_dir();
    let mut installed = Vec::new();
    for (name, bytes) in [("graph", graph_pack), ("attributes", attributes_pack)] {
        let source = pack_dir.join(format!("{name}.pack"));
        std::fs::write(&source, bytes).expect("write sibling pack");
        installed.push(
            install_pack_file_locally(&pack_dir, &source, name, 0, false)
                .await
                .expect("install sibling pack")
                .pack_path,
        );
    }

    validate_fetched_ref_tips(
        pack_dir
            .parent()
            .and_then(Path::parent)
            .expect("git directory"),
        &[commit_sha],
    )
    .await
    .expect("requested cross-pack tip must resolve");
    assert_eq!(installed.len(), 2);
}

/// Git records a missing parent as valid only at an intentional shallow boundary.
#[tokio::test]
async fn fetch_honors_shallow_commit_boundary() {
    let Some(repo) = Repo::new() else { return };

    let tree = match Command::new("git")
        .current_dir(&repo.work)
        .arg("mktree")
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
        _ => return,
    };
    let missing_parent = "1".repeat(40);
    let body = format!(
        "tree {tree}\nparent {missing_parent}\nauthor Test <test@example.com> 1700000000 +0000\ncommitter Test <test@example.com> 1700000000 +0000\n\nshallow boundary\n"
    );
    let Some(commit) = repo.write_literal("commit", body.as_bytes()) else {
        return;
    };
    let Some(pack) = repo.pack_exact_objects(&[&commit, &tree]) else {
        return;
    };

    let (unmarked_repo, unmarked_pack_dir) = temp_git_pack_dir();
    let unmarked_source = unmarked_pack_dir.join("source.pack");
    std::fs::write(&unmarked_source, &pack).expect("write unmarked pack");
    install_pack_file_locally(&unmarked_pack_dir, &unmarked_source, "unmarked", 0, false)
        .await
        .expect("install unmarked pack");
    validate_fetched_ref_tips(unmarked_repo.path(), std::slice::from_ref(&commit))
        .await
        .expect("bounded fetch validation only requires the requested tip");
    assert!(run_git_silent(
        unmarked_repo.path(),
        &["update-ref", "refs/heads/main", &commit]
    ));
    assert!(
        !run_git_silent(unmarked_repo.path(), &["fsck", "--connectivity-only"]),
        "independent Git validation must reject an unmarked missing parent"
    );

    let (shallow_repo, shallow_pack_dir) = temp_git_pack_dir();
    let shallow_source = shallow_pack_dir.join("source.pack");
    std::fs::write(&shallow_source, pack).expect("write shallow pack");
    install_pack_file_locally(&shallow_pack_dir, &shallow_source, "shallow", 0, false)
        .await
        .expect("install shallow pack");
    std::fs::write(shallow_repo.path().join("shallow"), format!("{commit}\n"))
        .expect("write shallow boundary");
    validate_fetched_ref_tips(shallow_repo.path(), std::slice::from_ref(&commit))
        .await
        .expect("requested shallow tip must resolve");
    assert!(run_git_silent(
        shallow_repo.path(),
        &["update-ref", "refs/heads/main", &commit]
    ));
    assert!(
        run_git_silent(shallow_repo.path(), &["fsck", "--connectivity-only"]),
        "an explicit shallow boundary must permit the absent parent"
    );
}

/// A valid Git pack may contain REF_DELTA entries whose base object is
/// itself deltified. Structural fetch indexing must preserve that chain.
#[tokio::test]
async fn fetch_accepts_deep_ref_delta_pack() {
    let Some(repo) = Repo::new() else { return };

    let root = repo.work.join("root");
    if std::fs::create_dir_all(&root).is_err() {
        return;
    }
    for i in 1..=80 {
        let path = root.join(format!("file{i:03}.txt"));
        if std::fs::write(path, format!("same prefix {i:04} same suffix\n")).is_err() {
            return;
        }
        run_git_silent(&repo.work, &["add", "root"]);
        if !run_git_silent(&repo.work, &["commit", "-m", &format!("delta {i}")]) {
            return;
        }
    }

    let Some(commits) = repo.all_commits() else {
        eprintln!("skipping: could not enumerate commits");
        return;
    };
    let commit_refs: Vec<&str> = commits.iter().map(String::as_str).collect();
    let Some(pack_bytes) =
        repo.pack_commits_with_args(&commit_refs, &["--window=50", "--depth=50"])
    else {
        eprintln!("skipping: could not build deep delta pack");
        return;
    };
    if repo.pack_has_ref_delta_chain(&pack_bytes) != Some(true) {
        eprintln!("skipping: git did not produce a deep REF_DELTA chain");
        return;
    }

    let (_dest, pack_dir) = temp_git_pack_dir();
    let canonical_name = "deep-ref-delta-pack";

    let source = pack_dir.join("deep-source.pack");
    std::fs::write(&source, pack_bytes).expect("write deep source pack");
    let installed = install_pack_file_locally(&pack_dir, &source, canonical_name, 0, false)
        .await
        .expect("deep delta pack install must succeed");
    validate_fetched_ref_tips(
        pack_dir
            .parent()
            .and_then(Path::parent)
            .expect("git directory"),
        std::slice::from_ref(&commits[0]),
    )
    .await
    .expect("requested deep-delta tip must resolve");

    assert!(installed.pack_path.exists());
    assert!(installed.idx_path.exists());
}
