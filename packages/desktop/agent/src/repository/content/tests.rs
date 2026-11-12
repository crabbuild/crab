use super::*;
use crate::shellout;
use tokio::io::AsyncWriteExt;

mod git_checksums;

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
    let repo = BrowseSession::open_local(root, &crate::repository::ReadScope::default())
        .await
        .unwrap();
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
    let source = FileSource::capture(&repo, "large.bin", None, repo.read_scope())
        .await
        .unwrap();
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
    let source = FileSource::capture(&repo, "file.txt", None, repo.read_scope())
        .await
        .unwrap();
    let replacement = directory.path().join("replacement");
    tokio::fs::write(&replacement, b"replacement")
        .await
        .unwrap();
    tokio::fs::rename(&replacement, &path).await.unwrap();
    assert_eq!(
        source.read_range(0, 8).await.unwrap(),
        b"captured".as_slice()
    );
    let source = FileSource::capture(&repo, "file.txt", None, repo.read_scope())
        .await
        .unwrap();
    tokio::fs::write(&path, b"short").await.unwrap();
    assert!(
        source
            .read_range(0, 4)
            .await
            .unwrap_err()
            .to_string()
            .contains("file changed")
    );
    tokio::fs::write(&path, b"").await.unwrap();
    let empty = FileSource::capture(&repo, "file.txt", None, repo.read_scope())
        .await
        .unwrap();
    tokio::fs::write(&path, b"grown").await.unwrap();
    assert!(empty.read_complete(0).await.is_err());
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
    let index = FileSource::capture(&repo, "file.txt", Some(""), repo.read_scope())
        .await
        .unwrap();
    let committed = FileSource::capture(&repo, "file.txt", Some("HEAD"), repo.read_scope())
        .await
        .unwrap();
    tokio::fs::write(&path, b"working").await.unwrap();
    git(root, &["add", "file.txt"]).await.unwrap();
    let working = FileSource::capture(&repo, "file.txt", None, repo.read_scope())
        .await
        .unwrap();
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
        FileSource::capture(&repo, "missing", Some("HEAD"), repo.read_scope())
            .await
            .is_err()
    );
    assert!(
        FileSource::capture(&repo, "../outside", None, repo.read_scope())
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_file_queries_cancel_independently_and_close_drains_readers() {
    let (directory, repo) = repository().await;
    let path = directory.path().join("file.txt");
    tokio::fs::write(&path, b"captured").await.unwrap();
    let source = FileSource::capture(&repo, "file.txt", None, repo.read_scope())
        .await
        .unwrap();
    let file = repo.files.insert("capture".into(), source).await.unwrap();
    let (guard, query) = file.query().await.unwrap();
    query.scope().cancel();
    assert!(query.read_range(0, 8).await.unwrap_err().is_cancelled());
    drop(guard);
    let replacement = directory.path().join("replacement");
    tokio::fs::write(&replacement, b"replacement")
        .await
        .unwrap();
    tokio::fs::rename(replacement, &path).await.unwrap();
    let (guard, query) = file.query().await.unwrap();
    assert_eq!(
        query.read_range(0, 8).await.unwrap(),
        b"captured".as_slice()
    );
    let closing = repo.clone();
    let close = tokio::spawn(async move { closing.close().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while query.check_cancelled().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!close.is_finished());
    let files = repo.files.clone();
    let duplicate_close = tokio::spawn(async move { files.remove("capture").await });
    tokio::task::yield_now().await;
    assert!(!duplicate_close.is_finished());
    drop(guard);
    close.await.unwrap().unwrap();
    duplicate_close.await.unwrap().unwrap();
    assert!(file.query().await.is_err());
    assert!(repo.files.get("capture").await.is_err());
}

pub(super) async fn working_pointer(
    kind: &str,
    size: u64,
) -> (tempfile::TempDir, BrowseSession, Bytes) {
    use std::io::Write as _;
    let (directory, repo) = repository().await;
    let root = directory.path();
    let binary = shellout::resolve_crab_bin();
    let driver = format!(
        "'{}' filter-process",
        binary.to_string_lossy().replace('\'', "'\\''")
    );
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
    std::fs::write(
        root.join(".gitattributes"),
        format!("*.bin filter={kind} -text\n"),
    )
    .unwrap();
    let mut file = std::fs::File::create(root.join("model[1],data.bin")).unwrap();
    file.write_all(b"retained").unwrap();
    file.set_len(size).unwrap();
    drop(file);
    git(root, &["add", "--", "model[1],data.bin"])
        .await
        .unwrap();
    let pointer = Bytes::from(git(root, &["show", ":model[1],data.bin"]).await.unwrap());
    std::fs::write(root.join("model[1],data.bin"), &pointer).unwrap();
    (directory, repo, pointer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
async fn materialization_streams_both_pointer_formats_above_the_complete_response_cap() {
    use std::io::Read as _;
    let size = MAX_COMPLETE_BYTES as u64 + 1024 * 1024;
    for kind in ["crab", "lfs"] {
        let (directory, repo, pointer) = working_pointer(kind, size).await;
        let index = FileSource::capture(&repo, "model[1],data.bin", Some(""), repo.read_scope())
            .await
            .unwrap();
        assert_eq!(index.describe()["can_materialize"], false);
        assert!(index.materialization(repo.read_scope()).is_err());
        index.close().await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                directory.path().join("model[1],data.bin"),
                std::fs::Permissions::from_mode(0o751),
            )
            .unwrap();
            std::os::unix::fs::symlink("model[1],data.bin", directory.path().join("link.bin"))
                .unwrap();
            let link = FileSource::capture(&repo, "link.bin", None, repo.read_scope())
                .await
                .unwrap();
            assert_eq!(link.describe()["can_materialize"], false);
            link.close().await.unwrap();
        }
        let source = FileSource::capture(&repo, "model[1],data.bin", None, repo.read_scope())
            .await
            .unwrap();
        let file = repo.files.insert("file".into(), source).await.unwrap();
        let (guard, source) = file.materialization(repo.read_scope()).await.unwrap();
        assert_eq!(source.materialize(|_| Ok(())).await.unwrap(), size);
        drop(guard);
        let mut bytes = std::fs::File::open(directory.path().join("model[1],data.bin")).unwrap();
        let mut prefix = [0; 8];
        bytes.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"retained");
        let mut buffer = vec![0; 1024 * 1024];
        let mut remaining = size - 8;
        loop {
            let count = bytes.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            assert!(buffer[..count].iter().all(|byte| *byte == 0));
            remaining -= count as u64;
        }
        assert_eq!(remaining, 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(directory.path().join("model[1],data.bin"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o751
            );
        }
        assert_eq!(
            git(directory.path(), &["show", ":model[1],data.bin"])
                .await
                .unwrap(),
            pointer
        );
        repo.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
async fn materialization_preserves_pointers_on_cancel_and_edits_observed_before_publication() {
    for kind in ["crab", "lfs"] {
        for edit in [false, true] {
            let size = 2 * 1024 * 1024;
            let (directory, repo, pointer) = working_pointer(kind, size).await;
            let path = directory.path().join("model[1],data.bin");
            let source = FileSource::capture(&repo, "model[1],data.bin", None, repo.read_scope())
                .await
                .unwrap();
            let file = repo.files.insert("file".into(), source).await.unwrap();
            let scope = repo.read_scope();
            let (guard, source) = file.materialization(scope.clone()).await.unwrap();
            let mut before = std::fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            before.sort();
            let target = path.clone();
            let result = source
                .materialize(move |written| {
                    if written == size {
                        if edit {
                            std::fs::write(&target, b"external edit")?;
                        } else {
                            scope.cancel();
                        }
                    }
                    Ok(())
                })
                .await;
            assert!(result.is_err());
            if !edit {
                assert!(result.unwrap_err().is_cancelled());
            }
            assert_eq!(
                std::fs::read(path).unwrap(),
                if edit {
                    b"external edit".as_slice()
                } else {
                    pointer.as_ref()
                }
            );
            let mut after = std::fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(after, before);
            drop(guard);
            repo.close().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
async fn dehydration_preserves_a_working_file_on_cancel_or_same_size_replacement() {
    let size = 2 * 1024 * 1024;
    let name = "model[1],data.bin";
    for replace in [false, true] {
        let (directory, repo, _) = working_pointer("lfs", size).await;
        let root = directory.path();
        git(root, &["add", "--", ".gitattributes"]).await.unwrap();
        git(
            root,
            &["-c", "commit.gpgsign=false", "commit", "-m", "Tracked file"],
        )
        .await
        .unwrap();
        let index = std::fs::read(root.join(".git/index")).unwrap();
        let path = root.join(name);
        let mut working = vec![0; size as usize];
        working[..8].copy_from_slice(b"retained");
        std::fs::write(&path, &working).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let alternate = root.join("replacement");
        let edited = vec![7; size as usize];
        let filenames = || {
            let mut names = std::fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        let before = filenames();
        let scope = repo.read_scope();
        let capture = Arc::new(
            Dehydration::capture(&repo, name, scope.clone())
                .await
                .unwrap(),
        );
        let outcome = capture
            .clone()
            .run(&repo, {
                let path = path.clone();
                let edited = edited.clone();
                move |checked| {
                    if checked == size * 2 {
                        if replace {
                            let mut replacement = std::fs::File::create(&alternate)?;
                            std::io::Write::write_all(&mut replacement, &edited)?;
                            replacement
                                .set_times(std::fs::FileTimes::new().set_modified(modified))?;
                            drop(replacement);
                            std::fs::rename(&alternate, &path)?;
                        } else {
                            scope.cancel();
                        }
                    }
                    Ok(())
                }
            })
            .await;
        capture.release_reader().await.unwrap();
        let error = outcome.unwrap_err();
        assert_eq!(error.is_cancelled(), !replace);
        assert_eq!(
            std::fs::read(path).unwrap(),
            if replace { edited } else { working }
        );
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(filenames(), before);
        repo.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
async fn materialization_outlives_its_view_and_repository_close_cancels_and_drains_it() {
    for close_repo in [false, true] {
        let (directory, repo, pointer) = working_pointer("lfs", 2 * 1024 * 1024).await;
        let source = FileSource::capture(&repo, "model[1],data.bin", None, repo.read_scope())
            .await
            .unwrap();
        let view_scope = source.scope();
        let file = repo.files.insert("file".into(), source).await.unwrap();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let scope = repo.read_scope();
        let observer = scope.clone();
        let job = tokio::spawn(async move {
            let (_guard, source) = file.materialization(scope).await.unwrap();
            let mut entered = Some(entered);
            source
                .materialize(move |_| {
                    if let Some(entered) = entered.take() {
                        entered.send(()).unwrap();
                        released.recv().unwrap();
                    }
                    Ok(())
                })
                .await
        });
        started.await.unwrap();
        let closing = repo.clone();
        let close = tokio::spawn(async move {
            if close_repo {
                closing.close().await
            } else {
                closing.files.remove("file").await
            }
        });
        if close_repo {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while observer.check().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while view_scope.check().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(observer.check().is_ok());
        }
        assert!(!close.is_finished());
        release.send(()).unwrap();
        let result = job.await.unwrap();
        close.await.unwrap().unwrap();
        assert!(repo.files.get("file").await.is_err());
        if close_repo {
            assert!(result.unwrap_err().is_cancelled());
            assert_eq!(
                std::fs::read(directory.path().join("model[1],data.bin")).unwrap(),
                pointer
            );
        } else {
            assert_eq!(result.unwrap(), 2 * 1024 * 1024);
        }
        repo.close().await.unwrap();
    }
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
    let size = 10 * 1024 * 1024;
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
        let source = FileSource::capture(&repo, path, None, repo.read_scope())
            .await
            .unwrap();
        let source = Arc::new(source);
        let reader = crate::preview::PreviewReader::new(source.clone());
        let parser = reader.clone();
        let preview = tokio::task::spawn_blocking(move || crate::preview::safetensors(parser))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(preview["tensors"][0]["nbytes"], size);
        assert_eq!(
            source.read_complete(bytes.len()).await.unwrap(),
            bytes.as_slice()
        );
        assert!(source.read_complete(1024).await.is_err());
        if path.starts_with("crab") {
            let mut start = 0;
            let mut offset = 0;
            loop {
                let page = source.chunk_page(start, 7).await.unwrap();
                assert_eq!(page.xorb_count, None);
                for chunk in &page.chunks {
                    assert_eq!(chunk.offset, offset);
                    offset += chunk.length;
                }
                let Some(next) = page.next else {
                    assert_eq!(start + page.chunks.len() as u64, page.chunk_count);
                    break;
                };
                assert!(next > start);
                start = next;
            }
            assert_eq!(offset, bytes.len() as u64);
        } else {
            assert!(source.chunk_page(0, 7).await.is_err());
        }
        source.close().await.unwrap();
        assert_eq!(std::fs::read(root.join(path)).unwrap(), pointer);
    }
    for pointer in [
        crab_types::pointer::Pointer {
            file_hash: [1; 32],
            size: 0,
            shard_hint: None,
        }
        .serialize(),
        // Canonical empty LFS serializes to an empty Git blob. Exercise the
        // accepted textual pointer shape with an incorrect empty-content hash.
        format!(
            "version {}\noid sha256:{}\nsize 0\n",
            crab_git::lfs_pointer::LFS_VERSION_URL,
            crab_git::lfs_pointer::hex_encode(&[1; 32])
        )
        .into_bytes(),
    ] {
        std::fs::write(root.join("empty.pointer"), &pointer).unwrap();
        let source = FileSource::capture(&repo, "empty.pointer", None, repo.read_scope())
            .await
            .unwrap();
        let result = source.read_complete(0).await;
        source.close().await.unwrap();
        assert!(matches!(result, Err(AgentError::Integrity { .. })));
    }
    let source = FileSource::capture(&repo, "crab.safetensors", None, repo.read_scope())
        .await
        .unwrap();
    let lfs = FileSource::capture(&repo, "lfs.safetensors", None, repo.read_scope())
        .await
        .unwrap();
    repo.close().await.unwrap();
    source.close().await.unwrap();
    lfs.close().await.unwrap();
    assert!(source.read_range(0, 8).await.is_err());
    assert!(lfs.read_range(0, 8).await.is_err());
    assert!(
        FileSource::capture(&repo, "crab.safetensors", None, repo.read_scope())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn captured_git_ranges_keep_physical_identity_and_support_large_complete_reads() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    let mut original = vec![b'a'; 9 * 1024 * 1024];
    original[..4].copy_from_slice(b"head");
    let size = original.len();
    original[size - 4..].copy_from_slice(b"tail");
    tokio::fs::write(root.join("file.bin"), &original)
        .await
        .unwrap();
    git(root, &["add", "file.bin"]).await.unwrap();
    let source = FileSource::capture(&repo, "file.bin", Some(""), repo.read_scope())
        .await
        .unwrap();
    let Source::Git { capture } = source.source.as_ref() else {
        panic!("expected captured Git blob");
    };
    let oid = &capture.oid;
    tokio::fs::write(root.join("file.bin"), b"replacement")
        .await
        .unwrap();
    git(root, &["add", "file.bin"]).await.unwrap();
    let replacement =
        String::from_utf8(git(root, &["rev-parse", ":file.bin"]).await.unwrap()).unwrap();
    git(root, &["replace", oid, replacement.trim()])
        .await
        .unwrap();
    let native_replacement = git(root, &["cat-file", "blob", oid]).await.unwrap();
    let prefix = source.read_range(0, 4).await.unwrap();
    let suffix = source.read_range(size as u64 - 4, 10).await.unwrap();
    let complete = source.read_complete(size).await.unwrap();
    assert_eq!(
        (
            native_replacement.as_slice(),
            prefix.as_ref(),
            suffix.as_ref(),
            complete.as_ref()
        ),
        (
            b"replacement".as_slice(),
            b"head".as_slice(),
            b"tail".as_slice(),
            original.as_slice()
        )
    );
    source.close().await.unwrap();
    repo.close().await.unwrap();
}
