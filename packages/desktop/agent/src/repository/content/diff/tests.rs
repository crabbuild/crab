use super::*;

async fn repository() -> (tempfile::TempDir, BrowseSession) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    for args in [
        vec!["init", "--initial-branch=main"],
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.invalid"],
    ] {
        super::super::super::git(root, &args).await.unwrap();
    }
    let repo = BrowseSession::open_local(root).await.unwrap();
    (directory, repo)
}

async fn commit(root: &Path, message: &str) {
    super::super::super::git(root, &["add", "--all"])
        .await
        .unwrap();
    super::super::super::git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-m", message],
    )
    .await
    .unwrap();
}

fn revision(rev: &str) -> Version {
    Version::Revision { rev: rev.into() }
}

async fn diff(repo: &BrowseSession, path: &str, before: Version, after: Version) -> Result<Value> {
    compare(
        repo,
        &Params {
            repo_id: "test".into(),
            path: path.into(),
            old_path: None,
            before,
            after,
            op_id: None,
        },
        repo.read_scope(),
    )
    .await
}

fn lfs(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
        crab_git::lfs_pointer::hex_encode(&sha2::Sha256::digest(bytes).into()),
        bytes.len()
    )
}

#[tokio::test]
async fn ordinary_git_diffs_distinguish_working_index_history_and_initial_commit() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    let path = "file[1],data.txt";
    tokio::fs::write(root.join(path), b"committed\n")
        .await
        .unwrap();
    let untracked = diff(&repo, path, revision("HEAD"), Version::Working)
        .await
        .unwrap();
    assert!(untracked["patch"].as_str().unwrap().contains("+committed"));
    commit(root, "first").await;
    let initial = diff(&repo, path, Version::Empty, revision("HEAD"))
        .await
        .unwrap();
    assert!(initial["patch"].as_str().unwrap().contains("+committed"));
    tokio::fs::write(root.join(path), b"staged\n")
        .await
        .unwrap();
    super::super::super::git(root, &["add", "--", path])
        .await
        .unwrap();
    tokio::fs::write(root.join(path), b"working\n")
        .await
        .unwrap();
    for (before, after, removed, added) in [
        (revision("HEAD"), Version::Index, "-committed", "+staged"),
        (Version::Index, Version::Working, "-staged", "+working"),
        (revision("HEAD"), Version::Working, "-committed", "+working"),
    ] {
        let result = diff(&repo, path, before, after).await.unwrap();
        let patch = result["patch"].as_str().unwrap();
        assert!(patch.contains(removed) && patch.contains(added), "{patch}");
    }
    let equal = diff(&repo, path, revision("HEAD"), revision("HEAD"))
        .await
        .unwrap();
    assert_eq!(equal, json!({ "kind": "git", "path": path, "patch": "" }));
    assert!(
        diff(&repo, path, revision("missing-revision"), Version::Working)
            .await
            .is_err()
    );
    repo.close().await.unwrap();
}

#[tokio::test]
async fn unconfigured_lfs_compares_same_size_hashes_without_opening_a_native_reader() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    tokio::fs::write(root.join("asset.bin"), lfs(b"before"))
        .await
        .unwrap();
    commit(root, "first").await;
    tokio::fs::write(root.join("asset.bin"), lfs(b"after!"))
        .await
        .unwrap();
    let changed = diff(&repo, "asset.bin", revision("HEAD"), Version::Working)
        .await
        .unwrap();
    assert_eq!(
        (
            changed["status"].clone(),
            changed["old_size"].clone(),
            changed["new_size"].clone(),
            changed["chunk_metrics"].clone()
        ),
        (json!("modified"), json!(6), json!(6), Value::Null)
    );
    tokio::fs::write(root.join("asset.bin"), b"before")
        .await
        .unwrap();
    let hydrated = diff(&repo, "asset.bin", revision("HEAD"), Version::Working)
        .await
        .unwrap();
    assert_eq!(hydrated["status"], "unchanged");
    tokio::fs::remove_file(root.join("asset.bin"))
        .await
        .unwrap();
    let deleted = diff(&repo, "asset.bin", revision("HEAD"), Version::Working)
        .await
        .unwrap();
    assert_eq!(deleted["status"], "deleted");
    repo.close().await.unwrap();
}

#[tokio::test]
async fn existing_empty_lfs_file_is_not_confused_with_an_absent_version() {
    let (directory, repo) = repository().await;
    tokio::fs::write(directory.path().join("empty.bin"), lfs(b""))
        .await
        .unwrap();
    commit(directory.path(), "empty").await;
    let added = diff(&repo, "empty.bin", Version::Empty, revision("HEAD"))
        .await
        .unwrap();
    assert_eq!(added["status"], "added");
    let same = diff(&repo, "empty.bin", revision("HEAD"), Version::Working)
        .await
        .unwrap();
    assert_eq!(same["status"], "unchanged");
    repo.close().await.unwrap();
}

#[tokio::test]
async fn different_crab_identities_do_not_fall_back_to_pointer_text_when_recipe_reads_fail() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    // Identity-only equality must work before storage has been configured.
    let pointer = |hash| {
        crab_types::pointer::Pointer {
            file_hash: [hash; 32],
            size: 100,
            shard_hint: None,
        }
        .serialize()
    };
    tokio::fs::write(root.join("asset.bin"), pointer(1))
        .await
        .unwrap();
    commit(root, "first").await;
    let same = diff(&repo, "asset.bin", revision("HEAD"), Version::Working)
        .await
        .unwrap();
    assert_eq!(same["status"], "unchanged");
    tokio::fs::write(root.join("asset.bin"), pointer(2))
        .await
        .unwrap();
    assert!(
        diff(&repo, "asset.bin", revision("HEAD"), Version::Working)
            .await
            .is_err()
    );
    // Failed recipe preparation is drained; it cannot leave an actor holding
    // repository ownership while a caller renders an ordinary Git patch.
    let _ = repo.close().await;
}

#[tokio::test]
async fn captured_working_chunk_sequence_uses_the_canonical_streaming_chunker() {
    let (directory, repo) = repository().await;
    let bytes = (0..(3 * 1024 * 1024_u32))
        .map(|value| value.wrapping_mul(2_654_435_761) as u8)
        .collect::<Vec<_>>();
    tokio::fs::write(directory.path().join("content.bin"), &bytes)
        .await
        .unwrap();
    let side = capture(&repo, "content.bin", Version::Working, repo.read_scope())
        .await
        .unwrap();
    let captured = sequence(&side).await.unwrap();
    let mut chunker = GearChunker::new();
    let mut expected = chunker.feed(&bytes);
    expected.extend(chunker.finalize());
    let actual = captured
        .spans
        .iter()
        .map(|chunk| (chunk.chunk_hash, chunk.len))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        expected
            .iter()
            .map(|chunk| (chunk.hash, chunk.data.len() as u64))
            .collect::<Vec<_>>()
    );
    side.close().await.unwrap();
    repo.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_drains_git_textconv_descendants() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    tokio::fs::write(root.join("content.txt"), b"before\n")
        .await
        .unwrap();
    commit(root, "first").await;
    tokio::fs::write(root.join("content.txt"), b"after\n")
        .await
        .unwrap();
    tokio::fs::write(root.join(".gitattributes"), "*.txt diff=slow\n")
        .await
        .unwrap();
    tokio::fs::write(
        root.join("helper.sh"),
        "sleep 60 &\necho $! > child.pid\nwait\n",
    )
    .await
    .unwrap();
    super::super::super::git(root, &["config", "diff.slow.textconv", "sh ./helper.sh"])
        .await
        .unwrap();
    let scope = repo.read_scope();
    let cancel = scope.clone();
    let params = Params {
        repo_id: "test".into(),
        path: "content.txt".into(),
        old_path: None,
        before: revision("HEAD"),
        after: Version::Working,
        op_id: None,
    };
    let work = compare(&repo, &params, scope);
    let cancelled = async {
        while !tokio::fs::try_exists(root.join("child.pid")).await.unwrap() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(work, cancelled)
    })
    .await
    .unwrap();
    assert!(result.unwrap_err().is_cancelled());
    repo.close().await.unwrap();
}

#[tokio::test]
async fn unborn_head_uses_an_empty_baseline_and_large_git_output_fails_explicitly() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    tokio::fs::write(root.join("content.txt"), b"initial\n")
        .await
        .unwrap();
    super::super::super::git(root, &["add", "content.txt"])
        .await
        .unwrap();
    let result = diff(&repo, "content.txt", revision("HEAD"), Version::Index)
        .await
        .unwrap();
    assert!(result["patch"].as_str().unwrap().contains("+initial"));
    commit(root, "first").await;
    tokio::fs::write(root.join("content.txt"), vec![b'x'; 9 * 1024 * 1024])
        .await
        .unwrap();
    let result = diff(&repo, "content.txt", revision("HEAD"), Version::Working).await;
    let error = result.unwrap_err();
    assert!(error.to_string().contains("exceeds 8 MiB"), "{error:?}");
    repo.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
async fn native_crab_comparison_uses_real_recipes_for_index_and_streams_the_working_version() {
    let size = 4 * 1024 * 1024_u64;
    let (directory, repo, original_pointer) =
        super::super::tests::working_pointer("crab", size).await;
    let root = directory.path();
    let path = "model[1],data.bin";
    super::super::super::git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-m", "first"],
    )
    .await
    .unwrap();
    let mut content = vec![0; size as usize];
    content[..8].copy_from_slice(b"retained");
    content[size as usize / 2] = 1;
    tokio::fs::write(root.join(path), &content).await.unwrap();
    super::super::super::git(root, &["add", "--", path])
        .await
        .unwrap();
    let index = diff(&repo, path, revision("HEAD"), Version::Index)
        .await
        .unwrap();
    let metrics = &index["chunk_metrics"];
    assert!(
        metrics["added_bytes"].as_u64().unwrap() > 0
            && metrics["removed_bytes"].as_u64().unwrap() > 0
            && metrics["unchanged_chunks"].as_u64().unwrap() > 1,
        "{index}"
    );
    assert_eq!(metrics["signed_delta_bytes"], 0);
    content[size as usize - 10] = 2;
    tokio::fs::write(root.join(path), content).await.unwrap();
    let working = diff(&repo, path, Version::Index, Version::Working)
        .await
        .unwrap();
    assert_eq!(
        (
            working["chunk_metrics"]["old_source"].clone(),
            working["chunk_metrics"]["new_source"].clone(),
            working["status"].clone()
        ),
        (json!("staged"), json!("worktree"), json!("modified"))
    );
    let added = diff(&repo, path, Version::Empty, revision("HEAD"))
        .await
        .unwrap();
    assert_eq!(
        (
            added["status"].clone(),
            added["chunk_metrics"]["added_bytes"].clone()
        ),
        (json!("added"), json!(size))
    );
    tokio::fs::write(root.join(path), original_pointer)
        .await
        .unwrap();
    let same = diff(&repo, path, revision("HEAD"), Version::Working)
        .await
        .unwrap();
    assert_eq!(same["status"], "unchanged");
    repo.close().await.unwrap();
}

#[tokio::test]
async fn notebook_comparisons_capture_versions_and_only_proven_absence_is_null() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    let path = "file[1].IPYNB";
    tokio::fs::write(root.join(path), b"committed")
        .await
        .unwrap();
    commit(root, "first").await;
    tokio::fs::write(root.join(path), b"staged").await.unwrap();
    super::super::super::git(root, &["add", "--", path])
        .await
        .unwrap();
    tokio::fs::write(root.join(path), b"").await.unwrap();
    for (before, after, old, new) in [
        (
            revision("HEAD"),
            Version::Index,
            json!("committed"),
            json!("staged"),
        ),
        (Version::Index, Version::Working, json!("staged"), json!("")),
        (
            Version::Empty,
            revision("HEAD"),
            Value::Null,
            json!("committed"),
        ),
    ] {
        let result = diff(&repo, path, before, after).await.unwrap();
        assert_eq!(
            result,
            json!({ "kind": "notebook", "path": path, "before": old, "after": new })
        );
    }
    assert!(
        diff(&repo, path, revision("missing-ref"), Version::Working)
            .await
            .is_err()
    );
    tokio::fs::remove_file(root.join(path)).await.unwrap();
    let deleted = diff(&repo, path, revision("HEAD"), Version::Working)
        .await
        .unwrap();
    assert!(deleted["after"].is_null());
    repo.close().await.unwrap();
}

#[tokio::test]
async fn notebook_comparisons_read_retained_working_and_index_content() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    let path = "retained.ipynb";
    tokio::fs::write(root.join(path), b"original")
        .await
        .unwrap();
    commit(root, "first").await;
    let scope = repo.read_scope();
    let before = capture(&repo, path, Version::Index, scope.clone())
        .await
        .unwrap();
    let after = capture(&repo, path, Version::Working, scope.clone())
        .await
        .unwrap();
    tokio::fs::rename(root.join(path), root.join("old.ipynb"))
        .await
        .unwrap();
    tokio::fs::write(root.join(path), b"replacement")
        .await
        .unwrap();
    super::super::super::git(root, &["add", "--", path])
        .await
        .unwrap();
    let result = comparison(
        &repo,
        &Params {
            repo_id: "test".into(),
            path: path.into(),
            old_path: None,
            before: Version::Index,
            after: Version::Working,
            op_id: None,
        },
        &before,
        &after,
        &scope,
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        json!({ "kind": "notebook", "path": path, "before": "original", "after": "original" })
    );
    before.close().await.unwrap();
    after.close().await.unwrap();
    repo.close().await.unwrap();
}

#[tokio::test]
async fn notebook_comparisons_reject_oversize_and_unavailable_pointer_content() {
    let (directory, repo) = repository().await;
    let root = directory.path();
    let path = "missing.ipynb";
    tokio::fs::write(root.join(path), lfs(b"unavailable content"))
        .await
        .unwrap();
    assert!(
        diff(&repo, path, Version::Empty, Version::Working)
            .await
            .is_err()
    );
    let file = std::fs::File::create(root.join(path)).unwrap();
    file.set_len(16 * 1024 * 1024 + 1).unwrap();
    let error = diff(&repo, path, Version::Empty, Version::Working)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("16 MiB combined input limit"));
    tokio::fs::write(root.join(path), [0xff, 0xfe])
        .await
        .unwrap();
    assert!(
        diff(&repo, path, Version::Empty, Version::Working)
            .await
            .is_err()
    );
    repo.close().await.unwrap();
}
