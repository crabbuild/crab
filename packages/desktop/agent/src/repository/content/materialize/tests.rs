use super::super::tests::working_pointer;
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
async fn missing_parent_materialization_preserves_index_and_concurrent_work() {
    let name = "models/deep/model[1],data.bin";
    let size = 2 * 1024 * 1024;
    for kind in ["crab", "lfs"] {
        for outcome in ["success", "cancel", "new-file", "index-change"] {
            let (directory, repo, _) = working_pointer(kind, size).await;
            let root = directory.path();
            std::fs::create_dir_all(root.join("models/deep")).unwrap();
            git(root, &["mv", "--", "model[1],data.bin", name])
                .await
                .unwrap();
            std::fs::write(root.join("other.txt"), "retain staged content").unwrap();
            git(root, &["add", "--", "other.txt"]).await.unwrap();
            #[cfg(unix)]
            git(root, &["update-index", "--chmod=+x", "--", name])
                .await
                .unwrap();
            let index = git(root, &["ls-files", "--stage", "-z"]).await.unwrap();
            let sibling = git(root, &["ls-files", "--stage", "-z", "--", "other.txt"])
                .await
                .unwrap();
            let replacement =
                String::from_utf8(git(root, &["rev-parse", ":other.txt"]).await.unwrap()).unwrap();
            std::fs::remove_dir_all(root.join("models")).unwrap();
            let scope = repo.read_scope();
            let source = Arc::new(
                FileSource::capture_materialization(&repo, name, scope.clone())
                    .await
                    .unwrap(),
            );
            assert!(!root.join("models").exists());
            let target = root.join(name);
            let edited = target.clone();
            let work_dir = root.to_path_buf();
            let mut observed = false;
            let result = source
                .clone()
                .materialize(move |written| {
                    if written == size && !observed {
                        observed = true;
                        // Reconstruction does not expose empty directory scaffolds.
                        assert!(!edited.parent().unwrap().exists());
                        match outcome {
                            "cancel" => scope.cancel(),
                            "new-file" => {
                                std::fs::create_dir_all(edited.parent().unwrap())?;
                                std::fs::write(&edited, "concurrent user content")?;
                            }
                            "index-change" => {
                                let status = std::process::Command::new("git")
                                    .args([
                                        "--literal-pathspecs",
                                        "update-index",
                                        "--cacheinfo",
                                        "100644",
                                        replacement.trim(),
                                        name,
                                    ])
                                    .current_dir(&work_dir)
                                    .status()?;
                                assert!(status.success());
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                })
                .await;
            source.release_reader().await.unwrap();
            match outcome {
                "success" => {
                    assert_eq!(result.unwrap(), size);
                    let mut content = vec![0; size as usize];
                    content[..8].copy_from_slice(b"retained");
                    assert_eq!(std::fs::read(&target).unwrap(), content);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        assert_ne!(
                            std::fs::metadata(&target).unwrap().permissions().mode() & 0o100,
                            0
                        );
                    }
                }
                "cancel" => {
                    assert!(result.unwrap_err().is_cancelled());
                    assert!(!root.join("models").exists());
                }
                "new-file" => {
                    assert!(result.is_err());
                    assert_eq!(std::fs::read(&target).unwrap(), b"concurrent user content");
                }
                _ => {
                    assert!(result.is_err());
                    assert!(!root.join("models").exists());
                    assert_eq!(
                        git(root, &["show", &format!(":{name}")]).await.unwrap(),
                        b"retain staged content"
                    );
                }
            }
            if outcome != "index-change" {
                assert_eq!(
                    git(root, &["ls-files", "--stage", "-z"]).await.unwrap(),
                    index
                );
            }
            assert_eq!(
                git(root, &["ls-files", "--stage", "-z", "--", "other.txt"])
                    .await
                    .unwrap(),
                sibling
            );
            assert!(!std::fs::read_dir(root).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".tmp")
            }));
            repo.close().await.unwrap();
        }
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
async fn missing_parent_materialization_rejects_symlink_redirection_before_publication() {
    let (directory, repo, _) = working_pointer("lfs", 2 * 1024 * 1024).await;
    let root = directory.path();
    std::fs::create_dir_all(root.join("models/deep")).unwrap();
    let name = "models/deep/model.bin";
    git(root, &["mv", "--", "model[1],data.bin", name])
        .await
        .unwrap();
    std::fs::remove_dir_all(root.join("models")).unwrap();
    let source = Arc::new(
        FileSource::capture_materialization(&repo, name, repo.read_scope())
            .await
            .unwrap(),
    );
    let outside = tempfile::tempdir().unwrap();
    let redirected = outside.path().to_owned();
    let parent = root.join("models");
    let result = source
        .clone()
        .materialize(move |written| {
            if written == 2 * 1024 * 1024 {
                std::os::unix::fs::symlink(&redirected, &parent)?;
            }
            Ok(())
        })
        .await;
    source.release_reader().await.unwrap();
    assert!(result.is_err());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    assert!(
        FileSource::capture_materialization(&repo, name, repo.read_scope())
            .await
            .is_err()
    );
    repo.close().await.unwrap();
}
