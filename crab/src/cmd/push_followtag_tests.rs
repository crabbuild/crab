use super::*;

fn git(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run Git");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git output")
        .trim()
        .to_owned()
}

fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("test repository");
    git(directory.path(), &["init", "-b", "main"]);
    git(directory.path(), &["config", "user.name", "Crab Test"]);
    git(
        directory.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    git(directory.path(), &["config", "commit.gpgsign", "false"]);
    commit(directory.path(), "first");
    directory
}

fn commit(repository: &Path, contents: &str) {
    std::fs::write(repository.join("tracked.txt"), contents).expect("write fixture");
    git(repository, &["add", "tracked.txt"]);
    git(repository, &["commit", "-m", contents]);
}

fn candidates(git_dir: &Path, source: &str) -> Vec<String> {
    collect_followtag_candidates(
        &[PushSpec {
            force: false,
            src: source.to_owned(),
            dst: "refs/heads/published".to_owned(),
        }],
        Some(git_dir),
    )
    .expect("collect tags")
    .into_iter()
    .map(|spec| spec.dst)
    .collect()
}

#[test]
fn followtags_use_selected_worktree_head_and_shared_tag_objects() {
    let repository = repository();
    git(repository.path(), &["tag", "-a", "first", "-m", "first"]);
    let first = git(repository.path(), &["rev-parse", "HEAD"]);
    commit(repository.path(), "second");
    git(repository.path(), &["tag", "-a", "second", "-m", "second"]);
    let linked_root = tempfile::tempdir().expect("linked root");
    let linked = linked_root.path().join("linked");
    git(
        repository.path(),
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().unwrap(),
            &first,
        ],
    );
    let context =
        crate::git::worktree::WorktreeContext::resolve_from_path(&linked).expect("linked context");
    assert_eq!(
        candidates(&context.per_worktree_git_dir, "HEAD"),
        ["refs/tags/first"]
    );
    let bare = linked_root.path().join("bare.git");
    git(
        linked_root.path(),
        &[
            "clone",
            "--bare",
            repository.path().to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    for git_dir in [repository.path().join(".git"), bare] {
        assert_eq!(
            candidates(&git_dir, "HEAD"),
            ["refs/tags/first", "refs/tags/second"]
        );
    }
}

#[test]
fn followtags_exclude_lightweight_unreachable_and_explicit_tags() {
    let repository = repository();
    git(
        repository.path(),
        &["tag", "-a", "ancestor", "-m", "ancestor"],
    );
    git(repository.path(), &["tag", "lightweight"]);
    commit(repository.path(), "second");
    let tree = git(repository.path(), &["rev-parse", "HEAD^{tree}"]);
    let orphan = git(
        repository.path(),
        &["commit-tree", &tree, "-m", "unreachable"],
    );
    git(
        repository.path(),
        &["tag", "-a", "outside", "-m", "outside", &orphan],
    );
    assert_eq!(
        candidates(&repository.path().join(".git"), "HEAD"),
        ["refs/tags/ancestor"]
    );
    let specs = [
        PushSpec {
            force: false,
            src: "HEAD".to_owned(),
            dst: "refs/heads/main".to_owned(),
        },
        PushSpec {
            force: false,
            src: "refs/tags/ancestor".to_owned(),
            dst: "refs/tags/ancestor".to_owned(),
        },
    ];
    assert!(
        collect_followtag_candidates(&specs, Some(&repository.path().join(".git")))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn followtags_reject_missing_annotated_tag_objects() {
    let repository = repository();
    git(repository.path(), &["tag", "-a", "broken", "-m", "broken"]);
    let oid = git(repository.path(), &["rev-parse", "refs/tags/broken"]);
    let git_dir = repository.path().join(".git");
    std::fs::remove_file(git_dir.join("objects").join(&oid[..2]).join(&oid[2..]))
        .expect("remove tag object");
    let error = collect_followtag_candidates(
        &[PushSpec {
            force: false,
            src: "HEAD".to_owned(),
            dst: "refs/heads/main".to_owned(),
        }],
        Some(&git_dir),
    )
    .unwrap_err();
    assert!(matches!(error, CrabError::GitTag(_)));
}

#[test]
fn followtags_accept_frozen_tag_sources_after_their_ref_is_deleted() {
    let repository = repository();
    git(
        repository.path(),
        &["tag", "-a", "ancestor", "-m", "ancestor"],
    );
    commit(repository.path(), "second");
    git(repository.path(), &["tag", "-a", "frozen", "-m", "frozen"]);
    let oid = git(repository.path(), &["rev-parse", "refs/tags/frozen"]);
    git(repository.path(), &["tag", "-d", "frozen"]);
    assert_eq!(
        candidates(&repository.path().join(".git"), &oid),
        ["refs/tags/ancestor"]
    );
}
