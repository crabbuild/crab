use super::*;
use base64::Engine as _;

async fn opened() -> (TempDir, PathBuf, State) {
    let (directory, root) = git_init_repo();
    let state = State::new();
    let repo = BrowseSession::open_local(&root, &crate::repository::ReadScope::default())
        .await
        .unwrap();
    state
        .insert("writer".into(), repo, Some(root.clone()))
        .await;
    (directory, root, state)
}

async fn write(state: &State, method: &str, path: &str, content: &[u8]) -> Result<Value> {
    let mut params = json!({ "repo_id": "writer", "path": path });
    if method == "write_text" {
        params["content"] = json!(std::str::from_utf8(content).unwrap());
    } else {
        params["bytes_b64"] = json!(base64::engine::general_purpose::STANDARD.encode(content));
    }
    handle(
        state,
        Request {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(method)),
            method: method.into(),
            params,
        },
        Arc::new(tokio::sync::Mutex::new(tokio::io::stdout())),
    )
    .await
}

#[tokio::test]
async fn native_file_writes_preserve_unrelated_same_stem_files() {
    let (_directory, root, state) = opened().await;
    let sibling = root.join("report.crab-tmp");
    std::fs::write(&sibling, b"UNRELATED_USER_FILE").unwrap();
    let mut results = Vec::new();
    for method in ["write_text", "write_bytes"] {
        let saved = write(&state, method, "report.txt", b"new content").await;
        results.push((saved.is_ok(), std::fs::read(&sibling).ok()));
    }
    state.shutdown().await.unwrap();
    assert_eq!(
        results,
        vec![(true, Some(b"UNRELATED_USER_FILE".to_vec())); 2]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn native_file_writes_preserve_executable_modes() {
    use std::os::unix::fs::PermissionsExt;

    let (_directory, root, state) = opened().await;
    let mut modes = Vec::new();
    for method in ["write_text", "write_bytes"] {
        let path = root.join(method);
        std::fs::write(&path, b"old content").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let saved = write(&state, method, method, b"new content").await;
        modes.push((
            saved.is_ok(),
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        ));
    }
    state.shutdown().await.unwrap();
    assert_eq!(modes, vec![(true, 0o751); 2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn native_file_writes_publish_complete_bytes_for_competing_same_stem_files() {
    let (_directory, root, state) = opened().await;
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..16 {
        let state = state.clone();
        tasks.spawn(async move {
            let content = vec![b'A' + index; 256 * 1024];
            let path = format!("report.part{index}");
            let method = if index % 2 == 0 {
                "write_text"
            } else {
                "write_bytes"
            };
            (
                path.clone(),
                content.clone(),
                write(&state, method, &path, &content).await,
            )
        });
    }
    let mut failures = Vec::new();
    while let Some(result) = tasks.join_next().await {
        let (path, expected, result) = result.unwrap();
        if result.is_err() || std::fs::read(root.join(&path)).ok().as_ref() != Some(&expected) {
            failures.push(path);
        }
    }
    state.shutdown().await.unwrap();
    assert!(
        failures.is_empty(),
        "failed complete publications: {failures:?}"
    );
}

#[tokio::test]
async fn native_file_writes_create_nested_empty_and_binary_files_without_temporary_residue() {
    let (_directory, root, state) = opened().await;
    let cases: [(&str, &[u8]); 4] = [
        ("write_text", "🦀 café".as_bytes()),
        ("write_text", b""),
        ("write_bytes", &[0, 255, 128, 10]),
        ("write_bytes", b""),
    ];
    let mut actual = Vec::new();
    for (index, (method, bytes)) in cases.into_iter().enumerate() {
        let path = format!("nested/new/file{index}");
        let saved = write(&state, method, &path, bytes).await.unwrap();
        actual.push((
            saved["size"].as_u64(),
            std::fs::read(root.join(path)).unwrap(),
        ));
    }
    let files = std::fs::read_dir(root.join("nested/new")).unwrap().count();
    state.shutdown().await.unwrap();
    let expected = cases.map(|(_, bytes)| (Some(bytes.len() as u64), bytes.to_vec()));
    assert!(actual == expected && files == 4);
}

#[tokio::test]
async fn native_file_writes_cannot_escape_the_repository_or_create_outside_parents() {
    let (directory, root, state) = opened().await;
    let outside = directory.path().join("outside");
    std::fs::write(&outside, b"UNCHANGED").unwrap();
    let absolute = outside.to_str().unwrap();
    let mut rejected = Vec::new();
    for method in ["write_text", "write_bytes"] {
        for path in ["../outside", absolute, "../new-parent/file.txt"] {
            rejected.push(write(&state, method, path, b"replacement").await.is_err());
        }
    }
    let unchanged = std::fs::read(&outside).unwrap();
    let created = directory.path().join("new-parent").exists();
    state.shutdown().await.unwrap();
    assert!(rejected.into_iter().all(|value| value) && unchanged == b"UNCHANGED" && !created);
}

#[tokio::test]
async fn native_file_writes_reject_readonly_targets_without_changing_content_or_permissions() {
    let (_directory, root, state) = opened().await;
    let mut actual = Vec::new();
    for method in ["write_text", "write_bytes"] {
        let path = root.join(method);
        std::fs::write(&path, b"UNCHANGED").unwrap();
        let original = std::fs::metadata(&path).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&path, readonly).unwrap();
        let rejected = write(&state, method, method, b"replacement").await.is_err();
        actual.push((
            rejected,
            std::fs::read(&path).unwrap(),
            std::fs::metadata(&path).unwrap().permissions().readonly(),
        ));
        std::fs::set_permissions(path, original).unwrap();
    }
    state.shutdown().await.unwrap();
    assert_eq!(actual, vec![(true, b"UNCHANGED".to_vec(), true); 2]);
}

#[cfg(unix)]
#[tokio::test]
async fn native_file_writes_follow_internal_links_without_replacing_them() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let (_directory, root, state) = opened().await;
    let target = root.join("target.txt");
    let link = root.join("link.txt");
    std::fs::write(&target, b"original").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o751)).unwrap();
    symlink("target.txt", &link).unwrap();
    let mut actual = Vec::new();
    for method in ["write_text", "write_bytes"] {
        let saved = write(&state, method, "link.txt", method.as_bytes())
            .await
            .is_ok();
        actual.push((
            saved,
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            std::fs::read(&target).unwrap(),
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        ));
    }
    state.shutdown().await.unwrap();
    assert_eq!(
        actual,
        ["write_text", "write_bytes"].map(|method| (true, true, method.as_bytes().to_vec(), 0o751))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn native_file_writes_reject_escaping_and_dangling_links_without_replacing_entries() {
    use std::os::unix::fs::symlink;

    let (directory, root, state) = opened().await;
    std::fs::write(directory.path().join("outside"), b"UNCHANGED").unwrap();
    symlink("../outside", root.join("external")).unwrap();
    symlink(directory.path(), root.join("parent")).unwrap();
    symlink("not-created", root.join("dangling")).unwrap();
    let mut rejected = Vec::new();
    for method in ["write_text", "write_bytes"] {
        for path in ["external", "parent/new-directory/file.txt", "dangling"] {
            rejected.push(write(&state, method, path, b"replacement").await.is_err());
        }
    }
    let links = ["external", "parent", "dangling"].map(|path| {
        std::fs::symlink_metadata(root.join(path))
            .unwrap()
            .is_symlink()
    });
    let outside = std::fs::read(directory.path().join("outside")).unwrap();
    let created =
        directory.path().join("new-directory").exists() || root.join("not-created").exists();
    state.shutdown().await.unwrap();
    assert!(
        rejected.into_iter().all(|value| value)
            && links.into_iter().all(|value| value)
            && outside == b"UNCHANGED"
            && !created
    );
}
