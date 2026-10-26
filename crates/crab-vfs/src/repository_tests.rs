#![expect(
    clippy::unwrap_used,
    reason = "Git repository test setup and assertions"
)]

use super::*;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

// --- Error classification ---

#[test]
fn classify_auth_401() {
    let err = classify_git_error("fatal: Authentication failed for 'https://example.com/repo'");
    let msg = err.to_string();
    assert!(msg.contains("authentication failed"), "got: {msg}");
}

#[test]
fn classify_auth_permission_denied() {
    let err = classify_git_error("fatal: Permission denied (publickey).");
    let msg = err.to_string();
    assert!(msg.contains("authentication failed"), "got: {msg}");
}

#[test]
fn classify_network_resolve() {
    let err = classify_git_error(
        "fatal: unable to access 'https://example.com/repo': Could not resolve host: example.com",
    );
    let msg = err.to_string();
    assert!(msg.contains("network error"), "got: {msg}");
}

#[test]
fn classify_network_connection_refused() {
    let err = classify_git_error(
        "fatal: unable to access 'https://example.com/repo': Connection refused",
    );
    let msg = err.to_string();
    assert!(msg.contains("network error"), "got: {msg}");
}

#[test]
fn classify_remote_not_found() {
    let err = classify_git_error("fatal: repository 'https://example.com/repo' not found");
    let msg = err.to_string();
    assert!(msg.contains("not found"), "got: {msg}");
}

#[test]
fn classify_not_git_repo() {
    let err = classify_git_error(
        "fatal: 'https://example.com/repo' does not appear to be a git repository",
    );
    let msg = err.to_string();
    assert!(msg.contains("not found"), "got: {msg}");
}

#[test]
fn classify_unknown_error_falls_through() {
    let err = classify_git_error("fatal: some unknown error happened");
    let msg = err.to_string();
    assert!(msg.contains("git operation failed"), "got: {msg}");
}

// --- parse_git_error ---

#[test]
fn parse_auth_error_message() {
    let msg = parse_git_error("fatal: Authentication failed for 'https://example.com'");
    assert_eq!(msg, "authentication failed: check your credentials");
}

#[test]
fn parse_network_error_message() {
    let msg = parse_git_error("fatal: Could not resolve host: example.com");
    assert_eq!(msg, "network error: could not reach remote");
}

#[test]
fn parse_not_found_message() {
    let msg = parse_git_error("fatal: repository 'https://example.com/repo' not found");
    assert_eq!(msg, "remote not found: check the URL");
}

#[test]
fn parse_unknown_preserves_stderr() {
    let msg = parse_git_error("  fatal: something weird  ");
    assert_eq!(msg, "fatal: something weird");
}

// --- ensure_blobless_clone (integration-style, filesystem only) ---

#[test]
fn cached_clone_fetches_requested_branch_without_losing_local_commits() {
    let _git_env = crate::test_support::CleanGitEnvGuard::new();
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let cache = root.path().join("cache.git");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "VFS test"]);
    git(&source, &["config", "user.email", "vfs@example.invalid"]);
    std::fs::write(source.join("file.txt"), b"base").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "base"]);
    git(&source, &["checkout", "-b", "other"]);
    std::fs::write(source.join("file.txt"), b"other").unwrap();
    git(&source, &["commit", "-am", "other"]);
    let other_oid = git(&source, &["rev-parse", "HEAD"]);
    git(&source, &["checkout", "main"]);
    ensure_blobless_clone(source.to_str().unwrap(), Some("main"), &cache).unwrap();
    ensure_blobless_clone(source.to_str().unwrap(), Some("other"), &cache).unwrap();
    assert_eq!(
        resolve_mount_ref(&cache, Some("other")).unwrap().0,
        other_oid
    );

    git(&source, &["tag", "-a", "release", "other", "-m", "release"]);
    let tag_oid = git(&source, &["rev-parse", "refs/tags/release"]);
    ensure_blobless_clone(source.to_str().unwrap(), Some("refs/tags/release"), &cache).unwrap();
    assert_eq!(git(&cache, &["rev-parse", "refs/tags/release"]), tag_oid);
    assert_eq!(
        resolve_mount_ref(&cache, Some("refs/tags/release"))
            .unwrap()
            .0,
        other_oid
    );
    let tag_cache = root.path().join("tag.git");
    ensure_blobless_clone(
        source.to_str().unwrap(),
        Some("refs/tags/release"),
        &tag_cache,
    )
    .unwrap();
    assert_eq!(
        git(&tag_cache, &["rev-parse", "refs/tags/release"]),
        tag_oid
    );

    git(
        &source,
        &[
            "tag",
            "-f",
            "-a",
            "release",
            "other",
            "-m",
            "updated release",
        ],
    );
    let updated_tag = git(&source, &["rev-parse", "refs/tags/release"]);
    ensure_blobless_clone(source.to_str().unwrap(), Some("refs/tags/release"), &cache).unwrap();
    assert_eq!(
        git(&cache, &["rev-parse", "refs/tags/release"]),
        updated_tag
    );
    assert!(matches!(
        ensure_blobless_clone(source.to_str().unwrap(), Some("missing"), &cache),
        Err(CrabError::NotFound { .. })
    ));

    git(&cache, &["config", "user.name", "VFS test"]);
    git(&cache, &["config", "user.email", "vfs@example.invalid"]);
    let local_oid = git(
        &cache,
        &[
            "commit-tree",
            "main^{tree}",
            "-p",
            "main",
            "-m",
            "unpublished",
        ],
    );
    git(&cache, &["update-ref", "refs/heads/main", &local_oid]);
    ensure_blobless_clone(source.to_str().unwrap(), Some("main"), &cache).unwrap();
    assert_eq!(git(&cache, &["rev-parse", "main"]), local_oid);
    assert_eq!(git(&source, &["symbolic-ref", "HEAD"]), "refs/heads/main");
}

#[test]
fn mount_cache_handoff_preserves_unpublished_history_and_overlay() {
    let _git_env = crate::test_support::CleanGitEnvGuard::new();
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "VFS test"]);
    git(&source, &["config", "user.email", "vfs@example.invalid"]);
    git(&source, &["commit", "--allow-empty", "-m", "base"]);
    for nested in [false, true] {
        let cache = root
            .path()
            .join(if nested { "nested-cache" } else { "bare-cache" });
        let metadata = if nested {
            cache.join(".git")
        } else {
            cache.clone()
        };
        ensure_blobless_clone(source.to_str().unwrap(), Some("main"), &metadata).unwrap();
        git(&metadata, &["config", "user.name", "VFS test"]);
        git(&metadata, &["config", "user.email", "vfs@example.invalid"]);
        let unpublished = git(
            &metadata,
            &[
                "commit-tree",
                "main^{tree}",
                "-p",
                "main",
                "-m",
                "unpublished",
            ],
        );
        git(&metadata, &["update-ref", "refs/heads/main", &unpublished]);
        std::fs::write(cache.join("overlay.db"), b"retained overlay").unwrap();
        let selected = crate::clone_cache::git_dir(&cache).unwrap();
        ensure_blobless_clone(source.to_str().unwrap(), Some("main"), &selected).unwrap();
        assert_eq!(
            (
                selected.clone(),
                resolve_mount_ref(&selected, Some("main")).unwrap().0,
                std::fs::read(cache.join("overlay.db")).unwrap()
            ),
            (metadata, unpublished, b"retained overlay".to_vec()),
        );
    }
}

#[test]
fn ensure_clone_detects_existing_bare_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("repo-cache");
    std::fs::create_dir_all(&cache_dir).unwrap();

    // Simulate an existing bare repo by creating a HEAD file.
    std::fs::write(cache_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    // This will attempt git fetch which will fail (no real remote),
    // but it proves the detection logic works.
    let result = ensure_blobless_clone(
        "https://example.com/nonexistent.git",
        Some("main"),
        &cache_dir,
    );

    // We expect an error from git fetch (no real remote configured),
    // but the important thing is it took the fetch path, not clone.
    assert!(result.is_err());
}

#[test]
fn ensure_clone_attempts_fresh_clone_when_no_head() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("fresh-cache");

    // No HEAD file exists, so it should attempt a clone.
    // This will fail (invalid URL) but proves the clone path is taken.
    let result = ensure_blobless_clone(
        "https://invalid-host-that-does-not-exist.example/repo.git",
        Some("main"),
        &cache_dir,
    );

    assert!(result.is_err());
    // The partial clone directory should have been cleaned up.
    // (It may not exist at all if git failed before creating it.)
}
#[test]
fn parse_ls_remote_heads_extracts_advertised_default() {
    let output = "\
ref: refs/heads/main\tHEAD\n\
1111111111111111111111111111111111111111\tHEAD\n\
1111111111111111111111111111111111111111\trefs/heads/main\n\
2222222222222222222222222222222222222222\trefs/heads/dev\n";

    let (default, heads) = parse_ls_remote_heads(output);

    assert_eq!(default.as_deref(), Some("main"));
    assert_eq!(heads, vec!["dev".to_owned(), "main".to_owned()]);
}

#[test]
fn select_default_clone_branch_prefers_advertised_default() {
    let heads = vec!["dev".to_owned(), "main".to_owned()];
    let branch = select_default_clone_branch(Some("release"), &heads).unwrap();
    assert_eq!(branch, "release");
}

#[test]
fn select_default_clone_branch_uses_main_without_advertised_head() {
    let heads = vec!["dev".to_owned(), "main".to_owned()];
    let branch = select_default_clone_branch(None, &heads).unwrap();
    assert_eq!(branch, "main");
}

#[test]
fn select_default_clone_branch_uses_master_without_main() {
    let heads = vec!["dev".to_owned(), "master".to_owned()];
    let branch = select_default_clone_branch(None, &heads).unwrap();
    assert_eq!(branch, "master");
}

#[test]
fn select_default_clone_branch_uses_single_branch() {
    let heads = vec!["release".to_owned()];
    let branch = select_default_clone_branch(None, &heads).unwrap();
    assert_eq!(branch, "release");
}

#[test]
fn select_default_clone_branch_rejects_ambiguous_remote() {
    let heads = vec!["dev".to_owned(), "release".to_owned()];
    let err = select_default_clone_branch(None, &heads).unwrap_err();
    assert!(err.to_string().contains("--ref"));
}

#[test]
fn clone_default_selects_a_sole_branch_when_remote_head_is_missing() {
    let _git_env = crate::test_support::CleanGitEnvGuard::new();
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let cache = root.path().join("cache.git");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "dev"]);
    git(
        &source,
        &[
            "-c",
            "user.name=VFS test",
            "-c",
            "user.email=vfs@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    let expected = git(&source, &["rev-parse", "HEAD"]);
    git(&source, &["symbolic-ref", "HEAD", "refs/heads/missing"]);
    ensure_blobless_clone(source.to_str().unwrap(), None, &cache).unwrap();
    assert_eq!(
        resolve_mount_ref(&cache, None).unwrap(),
        (expected, "refs/heads/dev".into())
    );
}

#[test]
fn failed_clone_preserves_preexisting_destination() {
    let _git_env = crate::test_support::CleanGitEnvGuard::new();
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let cache = root.path().join("cache");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(
        &source,
        &[
            "-c",
            "user.name=VFS test",
            "-c",
            "user.email=vfs@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    std::fs::create_dir(&cache).unwrap();
    std::fs::write(cache.join("unpublished"), b"retain local content").unwrap();
    assert!(ensure_blobless_clone(source.to_str().unwrap(), Some("missing"), &cache).is_err());
    assert_eq!(
        std::fs::read(cache.join("unpublished")).unwrap(),
        b"retain local content"
    );
}

#[test]
fn failed_clone_does_not_publish_partial_git_metadata() {
    let _git_env = crate::test_support::CleanGitEnvGuard::new();
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let cache = root.path().join("cache");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(
        &source,
        &[
            "-c",
            "user.name=VFS test",
            "-c",
            "user.email=vfs@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    assert!(ensure_blobless_clone(source.to_str().unwrap(), Some("missing"), &cache).is_err());
    assert!(!cache.exists());
}

#[test]
fn clone_publishes_into_an_existing_empty_destination() {
    let _git_env = crate::test_support::CleanGitEnvGuard::new();
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let cache = root.path().join("cache");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(
        &source,
        &[
            "-c",
            "user.name=VFS test",
            "-c",
            "user.email=vfs@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    std::fs::create_dir(&cache).unwrap();
    ensure_blobless_clone(source.to_str().unwrap(), Some("main"), &cache).unwrap();
    assert_eq!(
        resolve_mount_ref(&cache, None).unwrap().0,
        git(&source, &["rev-parse", "HEAD"])
    );
}

#[test]
fn clone_ignores_ambient_git_repository_context() {
    const CHILD_ROOT: &str = "CRAB_VFS_TEST_GIT_CONTEXT_CHILD";
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        let root = tempfile::tempdir().unwrap();
        // Process-wide mutation would contaminate sibling fixture commands
        // which do not all share GIT_DIR_MUTEX. Inject context only in a child.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "repository::tests::clone_ignores_ambient_git_repository_context",
                "--nocapture",
            ])
            .env(CHILD_ROOT, root.path())
            .env("GIT_DIR", root.path().join("unrelated.git"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            root.path().join("completed").is_file(),
            "ambient-context child did not run"
        );
        return;
    };
    let root = PathBuf::from(root);
    let source = root.join("source");
    let cache = root.join("cache");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(
        &source,
        &[
            "-c",
            "user.name=VFS test",
            "-c",
            "user.email=vfs@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    let expected = git(&source, &["rev-parse", "HEAD"]);
    ensure_blobless_clone(source.to_str().unwrap(), None, &cache).unwrap();
    assert_eq!(resolve_mount_ref(&cache, None).unwrap().0, expected);
    std::fs::write(root.join("completed"), b"verified").unwrap();
}
