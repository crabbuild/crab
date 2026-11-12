use super::*;

async fn indexed_git_source() -> Result<(tempfile::TempDir, BrowseSession, FileSource)> {
    let (directory, repo) = repository().await;
    let root = directory.path();
    std::fs::write(root.join("file.bin"), vec![b'a'; 1024 * 1024])?;
    let oid =
        String::from_utf8(git(root, &["hash-object", "--no-filters", "-w", "file.bin"]).await?)?;
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
    .await?;
    let source = FileSource::capture(&repo, "file.bin", Some(""), repo.read_scope()).await?;
    Ok((directory, repo, source))
}

async fn corrupted_git_source() -> Result<(tempfile::TempDir, BrowseSession, FileSource)> {
    let (directory, repo, source) = indexed_git_source().await?;
    let root = directory.path();
    let Source::Git { oid, .. } = source.source.as_ref() else {
        return Err(AgentError::Other("expected a captured Git blob".into()));
    };
    std::fs::write(root.join("replacement.bin"), vec![b'b'; 1024 * 1024])?;
    let replacement = String::from_utf8(
        git(
            root,
            &["hash-object", "--no-filters", "-w", "replacement.bin"],
        )
        .await?,
    )?;
    let replacement = replacement.trim();
    let object = root.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
    #[cfg(windows)]
    {
        let mut permissions = std::fs::metadata(&object)?.permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&object, permissions)?;
    }
    std::fs::remove_file(&object)?;
    let other = root
        .join(".git/objects")
        .join(&replacement[..2])
        .join(&replacement[2..]);
    std::fs::copy(other, object)?;
    Ok((directory, repo, source))
}

#[tokio::test]
async fn captured_git_blob_rejects_wrong_object_bytes_in_complete_read() -> Result<()> {
    let (_directory, repo, source) = corrupted_git_source().await?;
    let result = source.read_complete(2 * 1024 * 1024).await;
    assert!(
        result.is_err(),
        "Git returned a different object's valid compressed body"
    );
    let error = result
        .err()
        .ok_or_else(|| std::io::Error::other("expected corrupt Git object"))?;
    assert!(
        matches!(&error, AgentError::Io(source) if source.kind() == std::io::ErrorKind::InvalidData)
    );
    assert!(error.to_string().contains("checksum differs"));
    source.close().await?;
    repo.close().await?;
    Ok(())
}

#[tokio::test]
async fn captured_git_blob_rejects_wrong_object_bytes_in_file_copy() -> Result<()> {
    let (_directory, repo, source) = corrupted_git_source().await?;
    let result = source.copy_complete_to_file(tempfile::tempfile()?).await;
    assert!(
        result.is_err(),
        "Git copied a different object's valid compressed body"
    );
    let error = result
        .err()
        .ok_or_else(|| std::io::Error::other("expected corrupt Git object"))?;
    assert!(
        matches!(&error, AgentError::Io(source) if source.kind() == std::io::ErrorKind::InvalidData)
    );
    assert!(error.to_string().contains("checksum differs"));
    source.close().await?;
    repo.close().await?;
    Ok(())
}

#[tokio::test]
async fn complete_git_file_copy_preserves_the_captured_bytes() -> Result<()> {
    let (_directory, repo, source) = indexed_git_source().await?;
    let output = tempfile::NamedTempFile::new()?;
    source
        .copy_complete_to_file(output.as_file().try_clone()?)
        .await?;
    assert_eq!(std::fs::read(output.path())?, vec![b'a'; 1024 * 1024]);
    source.close().await?;
    repo.close().await?;
    Ok(())
}

#[tokio::test]
async fn small_git_pointer_is_authenticated_before_classification() -> Result<()> {
    let (directory, repo) = repository().await;
    let root = directory.path();
    let pointer = |byte: u8| {
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 17\n",
            char::from(byte).to_string().repeat(64)
        )
    };
    for (file, byte) in [("file.bin", b'a'), ("replacement.bin", b'b')] {
        std::fs::write(root.join(file), pointer(byte))?;
        git(root, &["add", "--", file]).await?;
    }
    let oid = String::from_utf8(git(root, &["rev-parse", ":file.bin"]).await?)?;
    let other = String::from_utf8(git(root, &["rev-parse", ":replacement.bin"]).await?)?;
    let object = |oid: &str| root.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
    let target = object(oid.trim());
    #[cfg(windows)]
    {
        let mut permissions = std::fs::metadata(&target)?.permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&target, permissions)?;
    }
    std::fs::remove_file(&target)?;
    std::fs::copy(object(other.trim()), target)?;
    let result = FileSource::capture_metadata(&repo, "file.bin", Some(""), repo.read_scope()).await;
    assert!(
        result.is_err(),
        "Unauthenticated pointer changed captured content identity"
    );
    assert!(
        result
            .err()
            .ok_or_else(|| std::io::Error::other("expected corrupt Git pointer"))?
            .to_string()
            .contains("checksum differs")
    );
    repo.close().await?;
    Ok(())
}
