use super::super::tests::working_pointer;
use super::*;

const NAME: &str = "models/deep/data[1].bin";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_progress_can_publish_on_the_async_runtime() {
    use std::sync::atomic::{AtomicU64, Ordering};
    for attributes in ["", "*.bin ident text eol=crlf\n"] {
        let body = b"INDEX\n$Id$\n".repeat(100_000);
        let (directory, repo) = ordinary(NAME, &body, attributes).await;
        let root = directory.path();
        let expected = git(root, &["cat-file", "--filters", &format!(":{NAME}")]);
        let expected_len = expected.len() as u64;
        std::fs::write(root.join(NAME), b"dirty").unwrap();
        let source = Arc::new(
            Restoration::capture(&repo, NAME, repo.read_scope())
                .await
                .unwrap(),
        );
        let reported = Arc::new(AtomicU64::new(0));
        let observed = reported.clone();
        let runtime = tokio::runtime::Handle::current();
        source
            .clone()
            .run(move |written| {
                // The transfer reporter synchronously publishes an async frame.
                runtime.block_on(async {
                    tokio::task::yield_now().await;
                    observed.store(written, Ordering::Release);
                    Ok(())
                })
            })
            .await
            .unwrap();
        source.release_reader().await.unwrap();
        assert_eq!(
            (
                std::fs::read(root.join(NAME)).unwrap(),
                reported.load(Ordering::Acquire)
            ),
            (expected, expected_len)
        );
        repo.close().await.unwrap();
    }
}

#[tokio::test]
async fn converted_restoration_refreshes_status_and_preserves_staging() {
    let (directory, repo) = ordinary(NAME, b"$Id$\noriginal\n", "").await;
    let root = directory.path();
    std::fs::write(root.join(".gitattributes"), "*.bin ident text eol=crlf\n").unwrap();
    let expected = git(root, &["cat-file", "--filters", &format!(":{NAME}")]);
    let staged = git(root, &["ls-files", "--stage", "-z"]);
    std::fs::write(root.join(NAME), b"dirty").unwrap();
    let source = Arc::new(
        Restoration::capture(&repo, NAME, repo.read_scope())
            .await
            .unwrap(),
    );
    source.run(|_| Ok(())).await.unwrap();
    assert_eq!(
        (
            std::fs::read(root.join(NAME)).unwrap(),
            git(
                root,
                &["--no-optional-locks", "status", "--porcelain", "--", NAME]
            ),
            git(root, &["ls-files", "--stage", "-z"]),
        ),
        (expected, Vec::new(), staged)
    );
    repo.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_matches_git_for_attributes_and_checkout_configuration() {
    let name = "models/deep/content[1].txt";
    for (attributes, autocrlf) in [
        ("", "false"),
        ("", "off"),
        ("", "input"),
        ("", "InPuT"),
        ("", "true"),
        ("*.txt diff\n", "false"),
        ("*.txt -text\n", "true"),
        ("*.txt text\n", "false"),
        ("*.txt text=unset\n", "true"),
        ("*.txt ident\n", "false"),
        ("*.txt -ident\n", "false"),
        ("*.txt ident=set\n", "false"),
        ("*.txt ident=unset\n", "false"),
        ("*.txt text eol=crlf\n", "false"),
        ("*.txt text eol=lf\n", "true"),
        ("*.txt crlf=input\n", "true"),
        ("*.txt crlf\n", "false"),
        ("*.txt crlf=auto\n", "false"),
        ("*.txt text=auto eol=crlf\n", "false"),
        ("*.txt text=auto\n", "false"),
        ("*.txt -text eol=crlf\n", "true"),
        ("*.txt -filter text eol=crlf\n", "false"),
        ("*.txt filter text eol=crlf\n", "false"),
        ("*.txt working-tree-encoding=UTF-16LE\n", "false"),
    ] {
        let (directory, repo) = ordinary(name, b"INDEX\n$Id$\n", "").await;
        let root = directory.path();
        std::fs::write(root.join(".gitattributes"), attributes).unwrap();
        git(root, &["add", "--", ".gitattributes"]);
        git(root, &["config", "core.autocrlf", autocrlf]);
        git(root, &["config", "core.eol", "crlf"]);
        let expected = git(root, &["cat-file", "--filters", &format!(":{name}")]);
        let index = git(root, &["ls-files", "--stage", "-z"]);
        std::fs::write(root.join(name), b"dirty").unwrap();
        let source = Arc::new(
            Restoration::capture(&repo, name, repo.read_scope())
                .await
                .unwrap(),
        );
        source.clone().run(|_| Ok(())).await.unwrap();
        source.release_reader().await.unwrap();
        assert_eq!(
            (
                std::fs::read(root.join(name)).unwrap(),
                git(root, &["ls-files", "--stage", "-z"]),
                scratch(root)
            ),
            (expected, index, Vec::new()),
            "attributes={attributes:?}, autocrlf={autocrlf}"
        );
        repo.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_progress_failure_retains_the_working_file() {
    for attributes in ["", "*.bin ident text eol=crlf\n"] {
        let (directory, repo) = ordinary(NAME, &b"$Id$\n".repeat(100_000), attributes).await;
        let root = directory.path();
        let index = std::fs::read(root.join(".git/index")).unwrap();
        std::fs::write(root.join(NAME), b"retain dirty work").unwrap();
        let source = Arc::new(
            Restoration::capture(&repo, NAME, repo.read_scope())
                .await
                .unwrap(),
        );
        let error = source
            .clone()
            .run(|_| {
                Err(AgentError::OpCancelled {
                    op_id: "progress-closed".into(),
                })
            })
            .await
            .unwrap_err();
        source.release_reader().await.unwrap();
        assert_eq!(
            (
                error.is_cancelled(),
                std::fs::read(root.join(NAME)).unwrap(),
                std::fs::read(root.join(".git/index")).unwrap(),
                scratch(root)
            ),
            (true, b"retain dirty work".to_vec(), index, Vec::new())
        );
        repo.close().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_keeps_filters_with_names_that_match_attribute_states() {
    let name = "models/deep/content[1].txt";
    for driver in ["set", "unset", "unspecified"] {
        let (directory, repo) = ordinary(name, b"INDEX\n", "").await;
        let root = directory.path();
        std::fs::write(
            root.join(".gitattributes"),
            format!("*.txt filter={driver}\n"),
        )
        .unwrap();
        git(root, &["add", "--", ".gitattributes"]);
        git(
            root,
            &[
                "config",
                &format!("filter.{driver}.smudge"),
                "sed 's/INDEX/WORK/g'",
            ],
        );
        git(
            root,
            &["config", &format!("filter.{driver}.required"), "true"],
        );
        std::fs::write(root.join(name), b"dirty").unwrap();
        let source = Arc::new(
            Restoration::capture(&repo, name, repo.read_scope())
                .await
                .unwrap(),
        );
        source.clone().run(|_| Ok(())).await.unwrap();
        source.release_reader().await.unwrap();
        assert_eq!(
            std::fs::read(root.join(name)).unwrap(),
            b"WORK\n",
            "{driver}"
        );
        repo.close().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_keeps_custom_smudge_filters_in_the_selected_storage_scope() {
    let name = "models/deep/filtered.txt";
    let (directory, repo) = ordinary(name, b"INDEX content\n", "*.txt text\n").await;
    let root = directory.path();
    git(
        root,
        &["config", "filter.demo.clean", "sed 's/WORK/INDEX/g'"],
    );
    git(
        root,
        &[
            "config",
            "filter.demo.smudge",
            "test \"$AWS_ACCESS_KEY_ID\" = fixture-access && test -z \"$AWS_SESSION_TOKEN\" && sed 's/INDEX/WORK/g'",
        ],
    );
    git(root, &["config", "filter.demo.required", "true"]);
    std::fs::write(root.join(".gitattributes"), "*.txt filter=demo -text\n").unwrap();
    git(root, &["add", "--", ".gitattributes"]);
    std::fs::write(root.join(name), "dirty").unwrap();
    let index = git(root, &["ls-files", "--stage", "-z"]);
    let connection = serde_json::from_value::<crate::storage_auth::Connection>(json!({
        "bucket": "fixture-bucket", "region": "us-east-1", "endpoint": "http://127.0.0.1:9",
        "credentials": { "provider": "s3", "access_key_id": "fixture-access", "secret_access_key": "fixture-secret", "session_token": null }
    })).unwrap();
    crate::storage_auth::scope(Some(connection), async {
        let source = Arc::new(
            Restoration::capture(&repo, name, repo.read_scope())
                .await
                .unwrap(),
        );
        source.clone().run(|_| Ok(())).await.unwrap();
        source.release_reader().await.unwrap();
    })
    .await;
    assert_eq!(
        (
            std::fs::read(root.join(name)).unwrap(),
            git(root, &["ls-files", "--stage", "-z"])
        ),
        (b"WORK content\n".to_vec(), index)
    );
    repo.close().await.unwrap();
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = std::process::Command::new("git")
        .arg("--literal-pathspecs")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("missing-global"))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

async fn ordinary(
    name: &str,
    content: &[u8],
    attributes: &str,
) -> (tempfile::TempDir, BrowseSession) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "--quiet", "--initial-branch=main"]);
    git(root, &["config", "user.name", "Restore fixture"]);
    git(root, &["config", "user.email", "restore@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
    std::fs::write(root.join(name), content).unwrap();
    std::fs::write(root.join("models/deep/data1.bin"), b"literal sibling").unwrap();
    std::fs::write(root.join("other.txt"), b"head sibling").unwrap();
    std::fs::write(root.join(".gitattributes"), attributes).unwrap();
    git(
        root,
        &[
            "add",
            "--",
            name,
            "models/deep/data1.bin",
            "other.txt",
            ".gitattributes",
        ],
    );
    git(root, &["commit", "--quiet", "-m", "Original index source"]);
    std::fs::write(root.join("other.txt"), b"retain unrelated staging").unwrap();
    git(root, &["add", "--", "other.txt"]);
    let repo = BrowseSession::open_local(root, &ReadScope::default())
        .await
        .unwrap();
    (directory, repo)
}

fn scratch(root: &Path) -> Vec<PathBuf> {
    let mut names = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() && entry.file_name() != ".git" {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".crab-publication-")
                {
                    names.push(entry.path());
                } else {
                    pending.push(entry.path());
                }
            }
        }
    }
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_uses_the_staged_version_and_literal_paths_without_changing_sibling_staging() {
    let original = vec![0; 2 * 1024 * 1024];
    let staged = vec![3; original.len()];
    let (directory, repo) = ordinary(NAME, &original, "*.bin -text\n").await;
    let root = directory.path();
    std::fs::write(root.join(NAME), &staged).unwrap();
    git(root, &["add", "--", NAME]);
    let index = git(root, &["ls-files", "--stage", "-z"]);
    std::fs::write(root.join(NAME), b"unstaged user changes").unwrap();
    let source = Arc::new(
        Restoration::capture(&repo, NAME, repo.read_scope())
            .await
            .unwrap(),
    );
    let oid = String::from_utf8(git(root, &["rev-parse", &format!(":{NAME}")])).unwrap();
    assert_eq!(
        serde_json::to_value(source.version().unwrap()).unwrap(),
        json!({ "kind": "git", "hash": oid.trim(), "size": staged.len(), "mode": "100644" })
    );
    assert_eq!(
        source.clone().run(|_| Ok(())).await.unwrap(),
        staged.len() as u64
    );
    source.release_reader().await.unwrap();
    assert_eq!(
        (
            std::fs::read(root.join(NAME)).unwrap(),
            git(root, &["ls-files", "--stage", "-z"]),
            git(root, &["show", &format!("HEAD:{NAME}")]),
            std::fs::read(root.join("models/deep/data1.bin")).unwrap(),
            scratch(root)
        ),
        (
            staged,
            index,
            original,
            b"literal sibling".to_vec(),
            Vec::new()
        )
    );
    repo.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_keeps_git_checkout_conversions_from_missing_working_attributes() {
    let name = "models/deep/data file[1].txt";
    let (directory, repo) = ordinary(name, b"index\ncontent\n", "*.txt text eol=crlf\n").await;
    let root = directory.path();
    let index = git(root, &["ls-files", "--stage", "-z"]);
    std::fs::remove_file(root.join(".gitattributes")).unwrap();
    std::fs::write(root.join(name), b"dirty").unwrap();
    let source = Arc::new(
        Restoration::capture(&repo, name, repo.read_scope())
            .await
            .unwrap(),
    );
    source.clone().run(|_| Ok(())).await.unwrap();
    source.release_reader().await.unwrap();
    assert_eq!(
        (
            std::fs::read(root.join(name)).unwrap(),
            git(root, &["ls-files", "--stage", "-z"])
        ),
        (b"index\r\ncontent\r\n".to_vec(), index)
    );
    repo.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_preserves_split_indexes_and_recreates_missing_parent_trees() {
    let content = vec![0; 1024 * 1024];
    let (directory, repo) = ordinary(NAME, &content, "*.bin -text\n").await;
    let root = directory.path();
    git(root, &["update-index", "--split-index"]);
    let index = git(root, &["ls-files", "--stage", "-z"]);
    std::fs::remove_dir_all(root.join("models")).unwrap();
    let source = Arc::new(
        Restoration::capture(&repo, NAME, repo.read_scope())
            .await
            .unwrap(),
    );
    source.clone().run(|_| Ok(())).await.unwrap();
    source.release_reader().await.unwrap();
    assert_eq!(
        (
            std::fs::read(root.join(NAME)).unwrap(),
            git(root, &["ls-files", "--stage", "-z"]),
            scratch(root)
        ),
        (content, index, Vec::new())
    );
    repo.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_rejects_concurrent_writers_cancellation_and_index_replacement() {
    for attributes in ["", "*.bin -text\n"] {
        for outcome in ["edit", "cancel", "index", "new-file"] {
            let content = vec![0; 1024 * 1024];
            let (directory, repo) = ordinary(NAME, &content, attributes).await;
            let root = directory.path();
            let dirty = b"original working changes";
            std::fs::write(root.join(NAME), dirty).unwrap();
            if outcome == "new-file" {
                std::fs::remove_dir_all(root.join("models")).unwrap();
            }
            let scope = repo.read_scope();
            let source = Arc::new(
                Restoration::capture(&repo, NAME, scope.clone())
                    .await
                    .unwrap(),
            );
            let target = root.join(NAME);
            let edited = target.clone();
            let work_dir = root.to_owned();
            let other = String::from_utf8(git(root, &["rev-parse", ":other.txt"])).unwrap();
            let sibling = git(root, &["ls-files", "--stage", "-z", "--", "other.txt"]);
            let mut changed = false;
            let result = source
                .clone()
                .run(move |written| {
                    if written == content.len() as u64 && !changed {
                        changed = true;
                        match outcome {
                            "cancel" => scope.cancel(),
                            "index" => {
                                git(
                                    &work_dir,
                                    &["update-index", "--cacheinfo", "100644", other.trim(), NAME],
                                );
                            }
                            _ => {
                                std::fs::create_dir_all(edited.parent().unwrap())?;
                                std::fs::write(&edited, b"concurrent user content")?;
                            }
                        }
                    }
                    Ok(())
                })
                .await;
            source.release_reader().await.unwrap();
            assert!(result.is_err(), "{outcome}");
            let expected = if matches!(outcome, "edit" | "new-file") {
                b"concurrent user content".as_slice()
            } else {
                dirty.as_slice()
            };
            assert_eq!(
                (
                    std::fs::read(target).unwrap(),
                    git(root, &["ls-files", "--stage", "-z", "--", "other.txt"]),
                    scratch(root)
                ),
                (expected.to_vec(), sibling, Vec::new())
            );
            repo.close().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_does_not_publish_when_the_index_object_is_unavailable_or_corrupt() {
    for attributes in [
        "",
        "*.bin -text\n",
        "*.bin text eol=crlf\n",
        "*.bin ident text=auto eol=crlf\n",
    ] {
        for fault in ["missing", "framing", "checksum"] {
            let content = b"row\n".repeat(256 * 1024);
            let (directory, repo) = ordinary(NAME, &content, attributes).await;
            let root = directory.path();
            let index = std::fs::read(root.join(".git/index")).unwrap();
            std::fs::write(root.join(NAME), b"retain working edits").unwrap();
            let source = Arc::new(
                Restoration::capture(&repo, NAME, repo.read_scope())
                    .await
                    .unwrap(),
            );
            let oid = String::from_utf8(git(root, &["rev-parse", &format!(":{NAME}")])).unwrap();
            let oid = oid.trim();
            let object = root.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
            std::fs::remove_file(&object).unwrap();
            match fault {
                "framing" => std::fs::write(&object, b"corrupt Git object").unwrap(),
                "checksum" => {
                    let mut changed = content;
                    *changed.last_mut().unwrap() ^= 1;
                    std::fs::write(root.join("replacement.bin"), changed).unwrap();
                    let replacement = String::from_utf8(git(
                        root,
                        &["hash-object", "--no-filters", "-w", "replacement.bin"],
                    ))
                    .unwrap();
                    let replacement = replacement.trim();
                    let bytes = std::fs::read(
                        root.join(".git/objects")
                            .join(&replacement[..2])
                            .join(&replacement[2..]),
                    )
                    .unwrap();
                    std::fs::write(&object, bytes).unwrap();
                }
                _ => {}
            }
            assert!(source.clone().run(|_| Ok(())).await.is_err());
            source.release_reader().await.unwrap();
            assert_eq!(
                (
                    std::fs::read(root.join(NAME)).unwrap(),
                    std::fs::read(root.join(".git/index")).unwrap(),
                    scratch(root)
                ),
                (b"retain working edits".to_vec(), index, Vec::new())
            );
            repo.close().await.unwrap();
        }
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoration_applies_index_executable_modes_and_rejects_links() {
    use std::os::unix::fs::PermissionsExt;
    for executable in [false, true] {
        let (directory, repo) = ordinary(NAME, b"index content", "*.bin -text\n").await;
        let root = directory.path();
        git(
            root,
            &[
                "update-index",
                if executable {
                    "--chmod=+x"
                } else {
                    "--chmod=-x"
                },
                "--",
                NAME,
            ],
        );
        std::fs::write(root.join(NAME), b"dirty").unwrap();
        std::fs::set_permissions(
            root.join(NAME),
            std::fs::Permissions::from_mode(if executable { 0o644 } else { 0o755 }),
        )
        .unwrap();
        let source = Arc::new(
            Restoration::capture(&repo, NAME, repo.read_scope())
                .await
                .unwrap(),
        );
        source.clone().run(|_| Ok(())).await.unwrap();
        source.release_reader().await.unwrap();
        assert_eq!(
            std::fs::metadata(root.join(NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o100
                != 0,
            executable
        );
        std::fs::remove_file(root.join(NAME)).unwrap();
        std::os::unix::fs::symlink(root.join("other.txt"), root.join(NAME)).unwrap();
        assert!(
            Restoration::capture(&repo, NAME, repo.read_scope())
                .await
                .is_err()
        );
        repo.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
async fn restoration_verifies_crab_and_lfs_index_content_and_preserves_concurrent_work() {
    let name = "model[1],data.bin";
    let size = 2 * 1024 * 1024;
    for kind in ["crab", "lfs"] {
        for outcome in ["success", "edit", "cancel", "index"] {
            let (directory, repo, _) = working_pointer(kind, size).await;
            let root = directory.path();
            std::fs::write(root.join("other.txt"), b"retain sibling staging").unwrap();
            git(root, &["add", "--", "other.txt"]);
            let index = git(root, &["ls-files", "--stage", "-z"]);
            let replacement = String::from_utf8(git(root, &["rev-parse", ":other.txt"])).unwrap();
            std::fs::write(root.join(name), b"working changes").unwrap();
            let scope = repo.read_scope();
            let source = Arc::new(
                Restoration::capture(&repo, name, scope.clone())
                    .await
                    .unwrap(),
            );
            let target = root.join(name);
            let edited = target.clone();
            let work_dir = root.to_owned();
            let mut changed = false;
            let result = source
                .clone()
                .run(move |written| {
                    if written == size && !changed {
                        changed = true;
                        match outcome {
                            "edit" => std::fs::write(&edited, b"concurrent user content")?,
                            "cancel" => scope.cancel(),
                            "index" => {
                                git(
                                    &work_dir,
                                    &[
                                        "update-index",
                                        "--cacheinfo",
                                        "100644",
                                        replacement.trim(),
                                        name,
                                    ],
                                );
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                })
                .await;
            source.release_reader().await.unwrap();
            if outcome == "success" {
                let mut expected = vec![0; size as usize];
                expected[..8].copy_from_slice(b"retained");
                assert_eq!(
                    (result.unwrap(), std::fs::read(target).unwrap()),
                    (size, expected)
                );
            } else {
                assert!(result.is_err());
                assert_eq!(
                    std::fs::read(target).unwrap(),
                    if outcome == "edit" {
                        b"concurrent user content".as_slice()
                    } else {
                        b"working changes".as_slice()
                    }
                );
            }
            if outcome != "index" {
                assert_eq!(git(root, &["ls-files", "--stage", "-z"]), index);
            }
            repo.close().await.unwrap();
        }
    }
}
