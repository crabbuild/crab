use super::*;

async fn indexed_git_source(
    bytes: &[u8],
) -> Result<(tempfile::TempDir, BrowseSession, FileSource)> {
    let (directory, repo) = repository().await;
    let root = directory.path();
    std::fs::write(root.join("file.bin"), bytes)?;
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

async fn corrupted_git_source(
    warm: bool,
) -> Result<(tempfile::TempDir, BrowseSession, FileSource)> {
    let original = vec![b'a'; 1024 * 1024];
    let (directory, repo, source) = indexed_git_source(&original).await?;
    if warm {
        assert_eq!(source.read_range(0, 32).await?.as_ref(), &original[..32]);
    }
    let root = directory.path();
    let Source::Git { oid, .. } = source.source.as_ref() else {
        return Err(AgentError::Other("expected a captured Git blob".into()));
    };
    let mut replacement = original;
    replacement[1024 * 1024 - 1] = b'b';
    std::fs::write(root.join("replacement.bin"), replacement)?;
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
    let (_directory, repo, source) = corrupted_git_source(false).await?;
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
    let (_directory, repo, source) = corrupted_git_source(false).await?;
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
async fn captured_git_ranges_reject_wrong_object_bytes() -> Result<()> {
    for (warm, offset) in [
        (false, 0),
        (false, 1024 * 1024 - 32),
        (true, 0),
        (true, 1024 * 1024 - 32),
    ] {
        let (_directory, repo, source) = corrupted_git_source(warm).await?;
        let result = source.read_range(offset, 32).await;
        assert!(
            result.is_err(),
            "A selected Git range exposed another object's bytes"
        );
        assert!(
            result
                .err()
                .ok_or_else(|| std::io::Error::other("expected corrupt Git range"))?
                .to_string()
                .contains("checksum differs")
        );
        source.close().await?;
        repo.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn complete_git_file_copy_preserves_the_captured_bytes() -> Result<()> {
    let original = vec![b'a'; 1024 * 1024];
    let (_directory, repo, source) = indexed_git_source(&original).await?;
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

#[tokio::test]
async fn verified_git_ranges_preserve_unaligned_bytes_and_clamp_at_eof() -> Result<()> {
    let body = (0..256 * 1024 + 17)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let (_directory, repo, source) = indexed_git_source(&body).await?;
    for (start, length) in [
        (0, 1),
        (63 * 1024 + 7, 4096),
        (128 * 1024 - 3, 10),
        (body.len() - 7, 100),
        (body.len(), 0),
    ] {
        let selected = source.read_range(start as u64, length as u32).await?;
        assert_eq!(
            selected.as_ref(),
            &body[start..(start + length).min(body.len())]
        );
    }
    source.close().await?;
    repo.close().await?;
    Ok(())
}

#[tokio::test]
async fn captured_git_reads_keep_exact_bytes_in_packs_and_alternate_stores() -> Result<()> {
    let body = (0..128 * 1024 + 17)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    for packed in [false, true] {
        let (directory, repo, source) = indexed_git_source(&body).await?;
        let root = directory.path();
        let oid = String::from_utf8(git(root, &["rev-parse", ":file.bin"]).await?)?;
        let oid = oid.trim();
        let loose = root.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
        if packed {
            git(
                root,
                &[
                    "pack-objects",
                    "--all",
                    "--indexed-objects",
                    ".git/objects/pack/pack",
                ],
            )
            .await?;
            git(root, &["prune-packed"]).await?;
        } else {
            let alternate = root.join("alternate store").join(&oid[..2]);
            std::fs::create_dir_all(&alternate)?;
            std::fs::rename(&loose, alternate.join(&oid[2..]))?;
            std::fs::write(
                root.join(".git/objects/info/alternates"),
                b"../../alternate store\n",
            )?;
        }
        assert!(
            !loose.exists(),
            "fixture must exercise the selected storage format"
        );
        assert_eq!(
            source.read_range(65533, 128).await?.as_ref(),
            &body[65533..65661]
        );
        assert_eq!(
            source.read_complete(body.len()).await?.as_ref(),
            body.as_slice()
        );
        let copy = tempfile::NamedTempFile::new()?;
        source
            .copy_complete_to_file(copy.as_file().try_clone()?)
            .await?;
        assert_eq!(std::fs::read(copy.path())?, body);
        source.close().await?;
        repo.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn captured_git_delta_ranges_and_copies_preserve_bytes_and_index() -> Result<()> {
    let (directory, repo) = repository().await;
    let root = directory.path();
    let mut bodies = [vec![b'x'; 1024 * 1024], vec![b'x'; 1024 * 1024]];
    bodies[1][1024 * 1024 - 1] = b'y';
    let names = ["base.bin", "target.bin"];
    let mut oids = Vec::new();
    for (name, bytes) in names.iter().zip(&bodies) {
        std::fs::write(root.join(name), bytes)?;
        let output = git(root, &["hash-object", "--no-filters", "-w", name]).await?;
        let oid = String::from_utf8(output)?.trim().to_owned();
        git(
            root,
            &["update-index", "--add", "--cacheinfo", "100644", &oid, name],
        )
        .await?;
        oids.push(oid);
    }
    let packed = git(
        root,
        &[
            "pack-objects",
            "--all",
            "--indexed-objects",
            "--delta-base-offset",
            "--threads=1",
            ".git/objects/pack/pack",
        ],
    )
    .await?;
    let packed = String::from_utf8(packed)?;
    let index_path = format!(".git/objects/pack/pack-{}.idx", packed.trim());
    let verification = String::from_utf8(git(root, &["verify-pack", "-v", &index_path]).await?)?;
    let delta = verification
        .lines()
        .find_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            (fields.len() == 7).then(|| fields[0].to_owned())
        })
        .ok_or_else(|| std::io::Error::other("fixture must contain an actual Git delta"))?;
    let selected = oids
        .iter()
        .position(|oid| oid == &delta)
        .ok_or_else(|| std::io::Error::other("fixture delta must identify a selected file"))?;
    git(root, &["prune-packed"]).await?;
    let index = std::fs::read(root.join(".git/index"))?;
    let source = FileSource::capture(&repo, names[selected], Some(""), repo.read_scope()).await?;
    assert_eq!(
        source.read_range(65533, 128).await?.as_ref(),
        &bodies[selected][65533..65661]
    );
    assert_eq!(
        source.read_complete(bodies[selected].len()).await?.as_ref(),
        bodies[selected].as_slice()
    );
    let output = tempfile::NamedTempFile::new()?;
    source
        .copy_complete_to_file(output.as_file().try_clone()?)
        .await?;
    assert_eq!(std::fs::read(output.path())?, bodies[selected]);
    assert_eq!(std::fs::read(root.join(".git/index"))?, index);
    source.close().await?;
    repo.close().await?;
    Ok(())
}
