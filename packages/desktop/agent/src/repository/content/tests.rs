use super::*;

async fn repository() -> (tempfile::TempDir, BrowseSession) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    for args in [
        vec!["init", "--initial-branch=main"],
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.invalid"],
    ] {
        git(root, &args).await.unwrap();
    }
    let repo = BrowseSession::open_local(root).await.unwrap();
    (directory, repo)
}

#[tokio::test]
async fn sparse_multi_gib_file_reads_only_requested_bytes_and_clamps_at_eof() {
    let (directory, repo) = repository().await;
    let size = 3 * 1024 * 1024 * 1024_u64;
    let mut file = tokio::fs::File::create(directory.path().join("large.bin"))
        .await
        .unwrap();
    file.set_len(size).await.unwrap();
    file.seek(std::io::SeekFrom::Start(size - 7)).await.unwrap();
    file.write_all(b"preview").await.unwrap();
    file.flush().await.unwrap();
    let source = FileSource::capture(&repo, "large.bin", None).await.unwrap();
    assert_eq!(
        (source.size, source.read_range(size - 7, 100).await.unwrap()),
        (size, Bytes::from_static(b"preview"))
    );
    assert!(source.read_range(u64::MAX, u32::MAX).await.is_err());
    assert!(source.read_range(u64::MAX, 100).await.unwrap().is_empty());
}

#[tokio::test]
async fn local_source_rejects_truncation_and_retains_opened_content_after_replacement() {
    let (directory, repo) = repository().await;
    let path = directory.path().join("file.txt");
    tokio::fs::write(&path, b"captured").await.unwrap();
    let source = FileSource::capture(&repo, "file.txt", None).await.unwrap();
    let replacement = directory.path().join("replacement");
    tokio::fs::write(&replacement, b"replacement")
        .await
        .unwrap();
    tokio::fs::rename(&replacement, &path).await.unwrap();
    assert_eq!(
        source.read_range(0, 8).await.unwrap(),
        b"captured".as_slice()
    );
    let source = FileSource::capture(&repo, "file.txt", None).await.unwrap();
    tokio::fs::write(&path, b"short").await.unwrap();
    assert!(
        source
            .read_range(0, 4)
            .await
            .unwrap_err()
            .to_string()
            .contains("file changed")
    );
}

#[tokio::test]
async fn index_and_committed_sources_remain_distinct_from_working_content() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    let path = root.join("file.txt");
    tokio::fs::write(&path, b"committed").await.unwrap();
    git(root, &["add", "file.txt"]).await.unwrap();
    git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-m", "initial"],
    )
    .await
    .unwrap();
    tokio::fs::write(&path, b"index").await.unwrap();
    git(root, &["add", "file.txt"]).await.unwrap();
    let index = FileSource::capture(&repo, "file.txt", Some(""))
        .await
        .unwrap();
    let committed = FileSource::capture(&repo, "file.txt", Some("HEAD"))
        .await
        .unwrap();
    tokio::fs::write(&path, b"working").await.unwrap();
    git(root, &["add", "file.txt"]).await.unwrap();
    let working = FileSource::capture(&repo, "file.txt", None).await.unwrap();
    assert_eq!(index.read_range(0, 10).await.unwrap(), b"index".as_slice());
    assert_eq!(
        committed.read_range(0, 10).await.unwrap(),
        b"committed".as_slice()
    );
    assert_eq!(
        working.read_range(0, 10).await.unwrap(),
        b"working".as_slice()
    );
    assert!(
        FileSource::capture(&repo, "missing", Some("HEAD"))
            .await
            .is_err()
    );
    assert!(
        FileSource::capture(&repo, "../outside", None)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
async fn native_crab_and_lfs_pointer_previews_use_staging_without_hydrating() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    let binary = shellout::resolve_crab_bin();
    let driver = format!(
        "'{}' filter-process",
        binary.to_string_lossy().replace('\'', "'\\''")
    );
    for kind in ["crab", "lfs"] {
        git(
            root,
            &["config", &format!("filter.{kind}.process"), &driver],
        )
        .await
        .unwrap();
        git(
            root,
            &["config", &format!("filter.{kind}.required"), "true"],
        )
        .await
        .unwrap();
    }
    std::fs::write(
        root.join(".gitattributes"),
        "crab.safetensors filter=crab -text\nlfs.safetensors filter=lfs -text\n",
    )
    .unwrap();
    let size = 2 * 1024 * 1024;
    let header = serde_json::to_vec(
        &json!({"weights": {"dtype":"U8", "shape":[size], "data_offsets":[0, size]}}),
    )
    .unwrap();
    let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
    bytes.extend(header);
    bytes.resize(bytes.len() + size, 42);
    for path in ["crab.safetensors", "lfs.safetensors"] {
        std::fs::write(root.join(path), &bytes).unwrap();
        git(root, &["add", path]).await.unwrap();
        let pointer = git(root, &["show", &format!(":{path}")]).await.unwrap();
        assert!(!matches!(
            crab_git::classify(&pointer),
            crab_git::PointerKind::NotAPointer
        ));
        std::fs::write(root.join(path), &pointer).unwrap();
        let source = FileSource::capture(&repo, path, None).await.unwrap();
        let reader = crate::preview::PreviewReader::new(source);
        let preview = tokio::task::spawn_blocking(move || crate::preview::safetensors(reader))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(preview["tensors"][0]["nbytes"], size);
        assert_eq!(std::fs::read(root.join(path)).unwrap(), pointer);
    }
}
