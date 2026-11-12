use super::*;

async fn indexed_git_source() -> (tempfile::TempDir, BrowseSession, FileSource) {
    let (directory, repo) = repository().await;
    let root = directory.path();
    std::fs::write(root.join("file.bin"), vec![b'a'; 1024 * 1024]).unwrap();
    let oid = String::from_utf8(
        git(root, &["hash-object", "--no-filters", "-w", "file.bin"])
            .await
            .unwrap(),
    )
    .unwrap();
    let oid = oid.trim();
    git(
        root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "100644",
            oid,
            "file.bin",
        ],
    )
    .await
    .unwrap();
    let source = FileSource::capture(&repo, "file.bin", Some(""), repo.read_scope())
        .await
        .unwrap();
    (directory, repo, source)
}

async fn corrupted_git_source() -> (tempfile::TempDir, BrowseSession, FileSource) {
    let (directory, repo, source) = indexed_git_source().await;
    let root = directory.path();
    let Source::Git { oid, .. } = source.source.as_ref() else {
        panic!("expected a captured Git blob");
    };
    std::fs::write(root.join("replacement.bin"), vec![b'b'; 1024 * 1024]).unwrap();
    let replacement = String::from_utf8(
        git(
            root,
            &["hash-object", "--no-filters", "-w", "replacement.bin"],
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let replacement = replacement.trim();
    let object = root.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
    #[cfg(windows)]
    {
        let mut permissions = std::fs::metadata(&object).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&object, permissions).unwrap();
    }
    std::fs::remove_file(&object).unwrap();
    let other = root
        .join(".git/objects")
        .join(&replacement[..2])
        .join(&replacement[2..]);
    std::fs::copy(other, object).unwrap();
    (directory, repo, source)
}

#[tokio::test]
async fn captured_git_blob_rejects_wrong_object_bytes_in_complete_read() {
    let (_directory, repo, source) = corrupted_git_source().await;
    let result = source.read_complete(2 * 1024 * 1024).await;
    assert!(
        result.is_err(),
        "Git returned a different object's valid compressed body"
    );
    let error = result.err().unwrap();
    assert!(
        matches!(&error, AgentError::Io(source) if source.kind() == std::io::ErrorKind::InvalidData)
    );
    assert!(error.to_string().contains("checksum differs"));
    source.close().await.unwrap();
    repo.close().await.unwrap();
}

#[tokio::test]
async fn captured_git_blob_rejects_wrong_object_bytes_in_file_copy() {
    let (_directory, repo, source) = corrupted_git_source().await;
    let result = source
        .copy_complete_to_file(tempfile::tempfile().unwrap())
        .await;
    assert!(
        result.is_err(),
        "Git copied a different object's valid compressed body"
    );
    let error = result.err().unwrap();
    assert!(
        matches!(&error, AgentError::Io(source) if source.kind() == std::io::ErrorKind::InvalidData)
    );
    assert!(error.to_string().contains("checksum differs"));
    source.close().await.unwrap();
    repo.close().await.unwrap();
}

#[tokio::test]
async fn complete_git_file_copy_preserves_the_captured_bytes() {
    let (_directory, repo, source) = indexed_git_source().await;
    let output = tempfile::NamedTempFile::new().unwrap();
    source
        .copy_complete_to_file(output.as_file().try_clone().unwrap())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(output.path()).unwrap(),
        vec![b'a'; 1024 * 1024]
    );
    source.close().await.unwrap();
    repo.close().await.unwrap();
}
