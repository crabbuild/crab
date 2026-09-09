#![cfg(all(feature = "write", feature = "content"))]

use std::{io::Write as _, path::PathBuf};

use crab_sdk::{
    Client, CommitIdentity, CommitOptions, ContentCache, DirectStoreOptions, EntryMode, ErrorKind,
    FileEdit, GitPath, MutationOutcome, OperationOptions, PageRequest, RefBatch, RefUpdate,
    RepositoryLocator, Revision, WritePolicy,
};
use futures_util::TryStreamExt as _;
use object_store::ObjectStoreExt as _;
use sha2::{Digest as _, Sha256};

fn live(prefix: &str) -> (Client, RepositoryLocator, tempfile::TempDir) {
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").unwrap();
    let suffix = uuid::Uuid::now_v7();
    let prefix = format!("qualification/{prefix}-{suffix}");
    let locator = RepositoryLocator::new(&prefix).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let cache = scratch.path().join("content-cache");
    std::fs::create_dir(&cache).unwrap();
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env(&bucket).unwrap())
        .content_cache(ContentCache::new(&cache, 256 * 1024 * 1024).unwrap())
        .build()
        .unwrap();
    (client, locator, scratch)
}

fn identity() -> CommitIdentity {
    CommitIdentity::new("SDK qualification", "sdk@example.invalid", 1_700_000_000, 0).unwrap()
}

async fn cleanup(locator: &RepositoryLocator) {
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").unwrap();
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    let prefix = object_store::path::Path::from(locator.prefix().unwrap());
    let objects = store
        .inner()
        .list(Some(&prefix))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    for object in objects {
        store.inner().delete(&object.location).await.unwrap();
    }
}

async fn initial_commit(
    client: &Client,
    locator: &RepositoryLocator,
    scratch: PathBuf,
) -> crab_sdk::ObjectId {
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap();
    let content = b"remote SDK\n".to_vec();
    let prepared = client
        .open_remote(locator.clone())
        .await
        .unwrap()
        .prepare_commit(
            CommitOptions::initial(
                "refs/heads/main",
                identity(),
                identity(),
                b"initial\n".to_vec(),
            )
            .unwrap(),
            vec![
                FileEdit::git(
                    GitPath::new("README.md").unwrap(),
                    EntryMode::Regular,
                    content.len() as u64,
                    std::io::Cursor::new(content),
                )
                .unwrap(),
            ],
            scratch,
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let commit = prepared.commit_id().unwrap();
    let outcome = prepared
        .execute(OperationOptions::default())
        .await
        .unwrap_or_else(|error| panic!("{}", error_chain(&error)));
    assert!(matches!(outcome, MutationOutcome::Committed { .. }));
    commit
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated writable S3 or RustFS qualification bucket"]
async fn remote_edit_round_trip_without_git() {
    const PHASE: &str = "CRAB_SDK_REMOTE_WRITE_CHILD";
    if std::env::var_os(PHASE).is_none() {
        let empty_path = tempfile::tempdir().unwrap();
        let exchange = tempfile::tempdir().unwrap();
        let result_path = exchange.path().join("result");
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "remote_edit_round_trip_without_git",
                "--ignored",
                "--nocapture",
            ])
            .env(PHASE, "1")
            .env("CRAB_SDK_REMOTE_WRITE_RESULT", &result_path)
            .env("PATH", empty_path.path())
            .env("GIT_EXEC_PATH", empty_path.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let result = std::fs::read_to_string(result_path).unwrap();
        let mut fields = result.lines();
        let locator = RepositoryLocator::new(fields.next().unwrap()).unwrap();
        let commit = crab_sdk::ObjectId::from_hex(fields.next().unwrap()).unwrap();
        assert!(fields.next().is_none());
        verify_independent_clone(&locator, commit);
        cleanup(&locator).await;
        return;
    }

    let (client, locator, scratch) = live("sdk-edit");
    let first = initial_commit(&client, &locator, scratch.path().to_owned()).await;
    let large_path = scratch.path().join("large.bin");
    let large_size = 65 * 1024 * 1024;
    let large_digest = write_large_fixture(&large_path, large_size);
    let prepared = client
        .open_remote(locator.clone())
        .await
        .unwrap()
        .prepare_commit(
            CommitOptions::new(
                first,
                "refs/heads/main",
                Some(first),
                identity(),
                identity(),
                b"large content\n".to_vec(),
            )
            .unwrap(),
            vec![
                FileEdit::delete(GitPath::new("README.md").unwrap()).unwrap(),
                FileEdit::hydrated(
                    GitPath::new("large.bin").unwrap(),
                    EntryMode::Executable,
                    large_size,
                    tokio::fs::File::open(&large_path).await.unwrap(),
                )
                .unwrap(),
            ],
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let second = prepared.commit_id().unwrap();
    let outcome = prepared
        .execute(OperationOptions::default())
        .await
        .unwrap_or_else(|error| panic!("{}", error_chain(&error)));
    assert!(matches!(outcome, MutationOutcome::Committed { .. }));
    let repo = client.open_remote(locator.clone()).await.unwrap();
    let tag = repo
        .prepare_ref_update(
            RefBatch::new(vec![RefUpdate::create("refs/tags/v1", second).unwrap()]).unwrap(),
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        tag.execute(OperationOptions::default()).await.unwrap(),
        MutationOutcome::Committed { .. }
    ));
    let delete_tag = client
        .open_remote(locator.clone())
        .await
        .unwrap()
        .prepare_ref_update(
            RefBatch::new(vec![RefUpdate::delete("refs/tags/v1", second).unwrap()]).unwrap(),
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        delete_tag
            .execute(OperationOptions::default())
            .await
            .unwrap(),
        MutationOutcome::Committed { .. }
    ));
    let snapshot = client
        .open_remote(locator.clone())
        .await
        .unwrap()
        .snapshot(Revision::commit(second))
        .await
        .unwrap();
    let tree = snapshot
        .tree(GitPath::root(), PageRequest::new(10, None).unwrap())
        .await
        .unwrap();
    assert_eq!(tree.items.len(), 1);
    assert_eq!(tree.items[0].mode, EntryMode::Executable);
    let mut stream = snapshot
        .open_file(GitPath::new("large.bin").unwrap())
        .await
        .unwrap();
    let mut hydrated_size = 0u64;
    let mut hydrated_digest = Sha256::new();
    while let Some(bytes) = stream.next().await.unwrap() {
        hydrated_size += bytes.len() as u64;
        hydrated_digest.update(&bytes);
    }
    assert_eq!(hydrated_size, large_size);
    assert_eq!(<[u8; 32]>::from(hydrated_digest.finalize()), large_digest);
    client.close().await.unwrap();
    if let Some(path) = std::env::var_os("CRAB_SDK_REMOTE_WRITE_RESULT") {
        std::fs::write(path, format!("{}\n{second}\n", locator.prefix().unwrap())).unwrap();
    } else {
        cleanup(&locator).await;
    }
}

fn verify_independent_clone(locator: &RepositoryLocator, commit: crab_sdk::ObjectId) {
    let directory = tempfile::tempdir().unwrap();
    let helper_directory = tempfile::tempdir().unwrap();
    let crab = std::env::var_os("CRAB_SDK_TEST_CRAB_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(format!("crab{}", std::env::consts::EXE_SUFFIX))
        });
    assert!(
        crab.is_file(),
        "set CRAB_SDK_TEST_CRAB_BIN to the built Crab executable"
    );
    let helper = helper_directory
        .path()
        .join(format!("git-remote-crab{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&crab, &helper).unwrap();
    let mut paths = vec![helper_directory.path().to_owned()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).unwrap();
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").unwrap();
    let destination = directory.path().join("clone");
    let git = std::env::var_os("CRAB_SDK_TEST_GIT_BIN").unwrap_or_else(|| "git".into());
    let remote = format!("crab://{bucket}/{}", locator.prefix().unwrap());
    let output = std::process::Command::new(&git)
        .args(["clone", "--no-checkout", &remote])
        .arg(&destination)
        .env("PATH", path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "independent clone: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let git_dir = destination.join(".git");
    let git_output = |args: &[&str]| {
        let output = std::process::Command::new(&git)
            .arg(format!("--git-dir={}", git_dir.display()))
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "independent Git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(
        git_output(&["rev-parse", "refs/heads/main"]).trim(),
        commit.to_string()
    );
    let entry = git_output(&["ls-tree", "refs/heads/main", "large.bin"]);
    assert!(entry.starts_with("100755 blob "));
    assert!(entry.ends_with("\tlarge.bin\n"));
    assert!(git_output(&["ls-tree", "refs/heads/main", "README.md"]).is_empty());
}

fn write_large_fixture(path: &std::path::Path, size: u64) -> [u8; 32] {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut offset = 0u64;
    while offset < size {
        let length = usize::try_from((size - offset).min(buffer.len() as u64)).unwrap();
        for (index, byte) in buffer[..length].iter_mut().enumerate() {
            let position = usize::try_from(offset).unwrap().wrapping_add(index);
            *byte = (position.wrapping_mul(73) ^ (position >> 8)) as u8;
        }
        file.write_all(&buffer[..length]).unwrap();
        digest.update(&buffer[..length]);
        offset += length as u64;
    }
    file.flush().unwrap();
    digest.finalize().into()
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut output = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        output.push_str(": ");
        output.push_str(&error.to_string());
        source = error.source();
    }
    output
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated writable S3 or RustFS qualification bucket"]
async fn stale_ref_batch_changes_no_refs() {
    let (client, locator, scratch) = live("sdk-stale");
    let first = initial_commit(&client, &locator, scratch.path().to_owned()).await;
    let next = b"next\n".to_vec();
    let prepared = client
        .open_remote(locator.clone())
        .await
        .unwrap()
        .prepare_commit(
            CommitOptions::new(
                first,
                "refs/heads/main",
                Some(first),
                identity(),
                identity(),
                b"advance\n".to_vec(),
            )
            .unwrap(),
            vec![
                FileEdit::git(
                    GitPath::new("next.txt").unwrap(),
                    EntryMode::Regular,
                    next.len() as u64,
                    std::io::Cursor::new(next),
                )
                .unwrap(),
            ],
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        prepared.execute(OperationOptions::default()).await.unwrap(),
        MutationOutcome::Committed { .. }
    ));
    let current = client
        .open_remote(locator.clone())
        .await
        .unwrap()
        .refs()
        .await
        .unwrap()
        .entries()
        .iter()
        .find(|reference| reference.name() == "refs/heads/main")
        .unwrap()
        .target();
    let stale = client
        .open_remote(locator.clone())
        .await
        .unwrap()
        .prepare_ref_update(
            RefBatch::new(vec![
                RefUpdate::create("refs/tags/must-not-exist", current).unwrap(),
                RefUpdate::update("refs/heads/main", current, first).unwrap(),
            ])
            .unwrap()
            .with_policy(WritePolicy::ForceWithLease),
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let competitor_content = b"competitor\n".to_vec();
    let competitor = client
        .open_remote(locator.clone())
        .await
        .unwrap()
        .prepare_commit(
            CommitOptions::new(
                current,
                "refs/heads/main",
                Some(current),
                identity(),
                identity(),
                b"competitor\n".to_vec(),
            )
            .unwrap(),
            vec![
                FileEdit::git(
                    GitPath::new("competitor.txt").unwrap(),
                    EntryMode::Regular,
                    competitor_content.len() as u64,
                    std::io::Cursor::new(competitor_content),
                )
                .unwrap(),
            ],
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        competitor
            .execute(OperationOptions::default())
            .await
            .unwrap(),
        MutationOutcome::Committed { .. }
    ));
    let outcome = stale.execute(OperationOptions::default()).await.unwrap();
    assert!(matches!(outcome, MutationOutcome::Rejected { .. }));
    let refs = client
        .open_remote(locator.clone())
        .await
        .unwrap()
        .refs()
        .await
        .unwrap();
    assert!(
        !refs
            .entries()
            .iter()
            .any(|reference| reference.name() == "refs/tags/must-not-exist")
    );
    assert_ne!(
        refs.entries()
            .iter()
            .find(|reference| reference.name() == "refs/heads/main")
            .unwrap()
            .target(),
        first
    );
    client.close().await.unwrap();
    cleanup(&locator).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated writable S3 or RustFS qualification bucket"]
async fn initialization_preserves_foreign_content() {
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").unwrap();
    let prefix = format!("qualification/sdk-foreign-{}", uuid::Uuid::now_v7());
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    let foreign = object_store::path::Path::from(format!("{prefix}/foreign"));
    store
        .inner()
        .put(&foreign, bytes::Bytes::from_static(b"keep").into())
        .await
        .unwrap();
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env(&bucket).unwrap())
        .build()
        .unwrap();
    let result = client
        .initialize_remote(RepositoryLocator::new(&prefix).unwrap(), "refs/heads/main")
        .await;
    assert!(matches!(result, Err(error) if error.kind() == ErrorKind::Corruption));
    assert_eq!(
        store
            .inner()
            .get(&foreign)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        b"keep"
    );
    let objects = store
        .inner()
        .list(Some(&object_store::path::Path::from(prefix)))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(objects.len(), 1);
    client.close().await.unwrap();
    store.inner().delete(&foreign).await.unwrap();
}
