#![cfg(feature = "local")]

use std::error::Error as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use crab_sdk::local::{
    CheckoutOptions, CloneOptions, CommitOptions as LocalCommitOptions, FetchDepth, FetchOptions,
    IntegrationKind, Options as LocalOptions, PullOptions, PullOutcome, PushOptions,
    PushOutcome as LocalPushOutcome, PushRefspec, Tools as LocalTools,
};
use crab_sdk::remote::EntryMode;
use crab_sdk::remote::write::{
    CommitIdentity, CommitOptions, FileEdit, MutationOutcome, RefBatch, RefUpdate,
};
use crab_sdk::storage::{ContentCache, DirectStoreOptions, S3Options};
use crab_sdk::{Client, GitPath, ObjectId, OpenOptions, RepositoryLocator};
use futures_util::TryStreamExt as _;
use object_store::ObjectStoreExt as _;
use sha2::{Digest as _, Sha256};

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn failure(error: &crab_sdk::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

fn live_client(scratch: &Path) -> (Client, RepositoryLocator) {
    let bucket = required("CRAB_SDK_TEST_S3_BUCKET");
    let endpoint = required("AWS_ENDPOINT_URL_S3");
    let options = S3Options::new(
        &bucket,
        &required("AWS_REGION"),
        &required("AWS_ACCESS_KEY_ID"),
        &required("AWS_SECRET_ACCESS_KEY"),
    )
    .unwrap()
    .with_endpoint(&endpoint);
    let cache = scratch.join("cache");
    std::fs::create_dir(&cache).unwrap();
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3(options))
        .content_cache(ContentCache::new(&cache, 512 * 1024 * 1024).unwrap())
        .local(LocalOptions::new(
            LocalTools::new(
                PathBuf::from(required("CRAB_SDK_TEST_GIT_BIN")),
                PathBuf::from(required("CRAB_SDK_TEST_CRAB_BIN")),
            )
            .unwrap(),
        ))
        .build()
        .unwrap();
    let prefix = format!("qualification/sdk-local-{}", uuid::Uuid::now_v7());
    let locator = RepositoryLocator::new(&prefix).unwrap();
    (client, locator)
}

fn identity() -> CommitIdentity {
    CommitIdentity::new("SDK qualification", "sdk@example.invalid", 1_700_000_000, 0).unwrap()
}

async fn initial_commit(client: &Client, locator: &RepositoryLocator, scratch: &Path) -> ObjectId {
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap_or_else(|error| panic!("{}", failure(&error)));
    let attributes = b"*.bin filter=crab diff=crab merge=crab -text\n".to_vec();
    let project = format!(
        "version = 1\n[remote]\nurl = \"crab://{}/{}\"\n",
        required("CRAB_SDK_TEST_S3_BUCKET"),
        locator.prefix().unwrap()
    )
    .into_bytes();
    let prepared = client
        .open(crab_sdk::OpenOptions::remote(locator.clone()))
        .await
        .unwrap()
        .remote()
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
                    GitPath::new(".gitattributes").unwrap(),
                    EntryMode::Regular,
                    attributes.len() as u64,
                    std::io::Cursor::new(attributes),
                )
                .unwrap(),
                FileEdit::git(
                    GitPath::new("README.md").unwrap(),
                    EntryMode::Regular,
                    8,
                    std::io::Cursor::new(b"initial\n".to_vec()),
                )
                .unwrap(),
                FileEdit::git(
                    GitPath::new("crab.toml").unwrap(),
                    EntryMode::Regular,
                    project.len() as u64,
                    std::io::Cursor::new(project),
                )
                .unwrap(),
            ],
            scratch.to_owned(),
        )
        .await
        .unwrap();
    let commit = prepared.commit_id().unwrap();
    assert!(matches!(
        prepared.execute().await.unwrap(),
        MutationOutcome::Committed { .. }
    ));
    commit
}

async fn advance_remote(
    client: &Client,
    locator: &RepositoryLocator,
    base: ObjectId,
    index: usize,
    scratch: &Path,
) -> ObjectId {
    let content = format!("remote revision {index}\n").into_bytes();
    let prepared = client
        .open(OpenOptions::remote(locator.clone()))
        .await
        .unwrap()
        .remote()
        .unwrap()
        .prepare_commit(
            CommitOptions::new(
                base,
                "refs/heads/main",
                Some(base),
                identity(),
                identity(),
                format!("remote revision {index}\n").into_bytes(),
            )
            .unwrap(),
            vec![
                FileEdit::git(
                    GitPath::new(format!("revision-{index}.txt")).unwrap(),
                    EntryMode::Regular,
                    content.len() as u64,
                    std::io::Cursor::new(content),
                )
                .unwrap(),
            ],
            scratch.to_owned(),
        )
        .await
        .unwrap();
    let commit = prepared.commit_id().unwrap();
    assert!(matches!(
        prepared.execute().await.unwrap(),
        MutationOutcome::Committed { .. }
    ));
    commit
}

async fn cleanup(locator: &RepositoryLocator) {
    let bucket = required("CRAB_SDK_TEST_S3_BUCKET");
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

async fn stored_objects(locator: &RepositoryLocator) -> Vec<(String, u64, Option<String>)> {
    let bucket = required("CRAB_SDK_TEST_S3_BUCKET");
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    let prefix = object_store::path::Path::from(locator.prefix().unwrap());
    let mut objects = store
        .inner()
        .list(Some(&prefix))
        .map_ok(|object| (object.location.to_string(), object.size, object.e_tag))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    objects.sort();
    objects
}

fn write_large_fixture(path: &Path, size: u64) -> [u8; 32] {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
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

fn file_digest(path: &Path) -> (u64, [u8; 32]) {
    let mut file = std::io::BufReader::new(std::fs::File::open(path).unwrap());
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        size += read as u64;
    }
    (size, digest.finalize().into())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated writable S3 or RustFS bucket and built Git/Crab executables"]
async fn stage_commit_push_round_trip() {
    let scratch = tempfile::tempdir().unwrap();
    let (client, locator) = live_client(scratch.path());
    eprintln!(
        "local qualification repository: {}",
        locator.prefix().unwrap()
    );
    initial_commit(&client, &locator, scratch.path()).await;

    let checkout = scratch.path().join("checkout");
    let repository = client
        .clone_local(locator.clone(), &checkout, CloneOptions::default())
        .await
        .unwrap_or_else(|error| panic!("{}", failure(&error)));
    let local = repository.local().unwrap();
    let large_size = 1024 * 1024 * 1024;
    std::fs::write(checkout.join("README.md"), b"updated locally\n").unwrap();
    let large_digest = write_large_fixture(&checkout.join("large.bin"), large_size);
    local
        .stage(vec!["README.md".into(), "large.bin".into()])
        .await
        .unwrap();
    let identity = self::identity();
    let local_commit = local
        .commit(
            LocalCommitOptions::new(identity.clone(), identity, b"local update\n".to_vec())
                .unwrap(),
        )
        .await
        .unwrap();
    let git = PathBuf::from(required("CRAB_SDK_TEST_GIT_BIN"));
    let tag = std::process::Command::new(&git)
        .current_dir(&checkout)
        .args(["tag", "sdk-round-trip"])
        .status()
        .unwrap();
    assert!(tag.success());
    let prepared = local
        .prepare_push(
            PushOptions::current_branch()
                .with_refspecs(vec![
                    PushRefspec::update("refs/heads/main", "refs/heads/main").unwrap(),
                    PushRefspec::update("refs/tags/sdk-round-trip", "refs/tags/sdk-round-trip")
                        .unwrap(),
                ])
                .unwrap(),
        )
        .await
        .unwrap();
    let persisted = prepared.recovery_token().to_json().unwrap();
    let pushed = prepared.execute().await.unwrap();
    assert!(
        matches!(pushed, LocalPushOutcome::Committed { .. }),
        "unexpected push outcome: {pushed:?}"
    );
    assert!(matches!(
        client
            .reconcile_local_push(
                crab_sdk::local::PushRecoveryToken::from_json(&persisted).unwrap(),
            )
            .await
            .unwrap(),
        LocalPushOutcome::Committed { .. }
    ));

    std::fs::write(checkout.join("dry-run.txt"), b"must stay local\n").unwrap();
    local.stage(vec!["dry-run.txt".into()]).await.unwrap();
    let identity = self::identity();
    local
        .commit(LocalCommitOptions::new(identity.clone(), identity, b"dry run\n".to_vec()).unwrap())
        .await
        .unwrap();
    let before_dry_run = stored_objects(&locator).await;
    let dry_run = local
        .prepare_push(PushOptions::current_branch().dry_run(true))
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert!(matches!(dry_run, LocalPushOutcome::DryRun { .. }));
    assert_eq!(stored_objects(&locator).await, before_dry_run);

    let verified = scratch.path().join("verified");
    let cloned_repository = client
        .clone_local(locator.clone(), &verified, CloneOptions::default().eager())
        .await
        .unwrap();
    let cloned = cloned_repository.local().unwrap();
    assert_eq!(
        std::fs::read(verified.join("README.md")).unwrap(),
        b"updated locally\n"
    );
    assert_eq!(
        file_digest(&verified.join("large.bin")),
        (large_size, large_digest)
    );
    let status = cloned.status().await.unwrap();
    assert!(
        status.is_clean(),
        "independent eager clone status: {status:?}"
    );
    let head = std::process::Command::new(required("CRAB_SDK_TEST_GIT_BIN"))
        .current_dir(&verified)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(head.status.success());
    assert_eq!(
        String::from_utf8(head.stdout).unwrap().trim(),
        local_commit.to_string()
    );
    assert_eq!(
        std::process::Command::new(&git)
            .current_dir(&verified)
            .args(["rev-parse", "refs/tags/sdk-round-trip"])
            .output()
            .map(|output| String::from_utf8(output.stdout).unwrap().trim().to_owned())
            .unwrap(),
        local_commit.to_string()
    );

    let configuration = client.configure_local(&verified).await.unwrap();
    assert!(!configuration.git_version().is_empty());
    assert!(!configuration.crab_version().is_empty());
    drop(cloned_repository);
    let reopened = client.open(OpenOptions::local(&verified)).await.unwrap();
    let reopened = reopened.local().unwrap();
    assert_eq!(reopened.path(), verified.canonicalize().unwrap());
    assert!(reopened.common_directory().ends_with(".git"));
    let snapshot = reopened.snapshot("HEAD").await.unwrap();
    assert_eq!(snapshot.commit_id(), local_commit);
    assert_eq!(snapshot.history(10).await.unwrap().len(), 2);
    reopened.dehydrate(vec!["large.bin".into()]).await.unwrap();
    assert!(std::fs::metadata(verified.join("large.bin")).unwrap().len() < 1024);
    reopened
        .prefetch_content(vec!["large.bin".into()])
        .await
        .unwrap();
    reopened.hydrate(vec!["large.bin".into()]).await.unwrap();
    assert_eq!(
        file_digest(&verified.join("large.bin")),
        (large_size, large_digest)
    );
    assert!(reopened.status().await.unwrap().is_clean());

    let deleted = local
        .prepare_push(
            PushOptions::current_branch()
                .with_refspecs(vec![
                    PushRefspec::delete("refs/tags/sdk-round-trip").unwrap(),
                ])
                .unwrap(),
        )
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert!(matches!(deleted, LocalPushOutcome::Committed { .. }));
    let refs = client
        .open(crab_sdk::OpenOptions::remote(locator.clone()))
        .await
        .unwrap()
        .remote()
        .unwrap()
        .refs()
        .await
        .unwrap();
    assert!(
        refs.entries()
            .iter()
            .all(|reference| reference.name() != "refs/tags/sdk-round-trip")
    );

    client.close().await.unwrap();
    cleanup(&locator).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated writable S3 or RustFS bucket and built Git/Crab executables"]
async fn fetch_checkout_and_pull_round_trip() {
    let scratch = tempfile::tempdir().unwrap();
    let (client, locator) = live_client(scratch.path());
    let first = initial_commit(&client, &locator, scratch.path()).await;
    let second = advance_remote(&client, &locator, first, 2, scratch.path()).await;
    let third = advance_remote(&client, &locator, second, 3, scratch.path()).await;
    let fourth = advance_remote(&client, &locator, third, 4, scratch.path()).await;
    let tag = client
        .open(OpenOptions::remote(locator.clone()))
        .await
        .unwrap()
        .remote()
        .unwrap()
        .prepare_ref_update(
            RefBatch::new(vec![
                RefUpdate::create("refs/tags/retained", fourth).unwrap(),
            ])
            .unwrap(),
            scratch.path().to_owned(),
        )
        .await
        .unwrap();
    assert!(matches!(
        tag.execute().await.unwrap(),
        MutationOutcome::Committed { .. }
    ));

    let checkout = scratch.path().join("shallow");
    let repository = client
        .clone_local(
            locator.clone(),
            &checkout,
            CloneOptions::default()
                .with_branch("main")
                .unwrap()
                .with_depth(1)
                .unwrap()
                .with_remote_name("upstream")
                .unwrap(),
        )
        .await
        .unwrap_or_else(|error| panic!("{}", failure(&error)));
    let local = repository.local().unwrap();
    let git = PathBuf::from(required("CRAB_SDK_TEST_GIT_BIN"));
    let git_output = |arguments: &[&str]| {
        let output = std::process::Command::new(&git)
            .current_dir(&checkout)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    assert_eq!(git_output(&["rev-list", "--count", "HEAD"]), "1");
    let deepened = local
        .fetch(
            FetchOptions::default()
                .with_remote("upstream")
                .unwrap()
                .with_depth(FetchDepth::Deepen(2))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(deepened.is_shallow());
    assert_eq!(deepened.head(), Some(fourth));
    assert_eq!(git_output(&["rev-list", "--count", "HEAD"]), "3");
    let complete = local
        .fetch(
            FetchOptions::default()
                .with_remote("upstream")
                .unwrap()
                .tags(true)
                .with_depth(FetchDepth::Unshallow)
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!complete.is_shallow());
    assert_eq!(git_output(&["rev-list", "--count", "HEAD"]), "4");
    assert_eq!(
        git_output(&["rev-parse", "refs/tags/retained"]),
        fourth.to_string()
    );

    git_output(&["update-ref", "refs/remotes/upstream/obsolete", "HEAD"]);
    git_output(&["update-ref", "refs/tags/obsolete", "HEAD"]);
    local
        .fetch(
            FetchOptions::default()
                .with_remote("upstream")
                .unwrap()
                .tags(true)
                .prune(true),
        )
        .await
        .unwrap();
    assert!(git_output(&["tag", "--list", "obsolete"]).is_empty());
    let missing = std::process::Command::new(&git)
        .current_dir(&checkout)
        .args(["show-ref", "--verify", "refs/remotes/upstream/obsolete"])
        .output()
        .unwrap();
    assert!(!missing.status.success());

    assert_eq!(
        local
            .checkout(
                "HEAD",
                CheckoutOptions::default().create_branch("work").unwrap(),
            )
            .await
            .unwrap(),
        fourth
    );
    assert_eq!(
        local
            .checkout("main", CheckoutOptions::default())
            .await
            .unwrap(),
        fourth
    );

    let fifth = advance_remote(&client, &locator, fourth, 5, scratch.path()).await;
    let pulled = local
        .pull(
            PullOptions::fast_forward_only()
                .with_remote("upstream")
                .unwrap()
                .hydrate(false),
        )
        .await
        .unwrap();
    assert!(matches!(pulled, PullOutcome::Updated { head, .. } if head == fifth));
    assert_eq!(git_output(&["rev-parse", "HEAD"]), fifth.to_string());

    let linked = scratch.path().join("linked");
    let linked_text = linked.to_str().unwrap();
    git_output(&["worktree", "add", "-b", "linked", linked_text]);
    let linked_repository = client.open(OpenOptions::local(&linked)).await.unwrap();
    let linked_local = linked_repository.local().unwrap();
    assert_eq!(linked_local.path(), linked.canonicalize().unwrap());
    assert_eq!(linked_local.common_directory(), local.common_directory());
    assert_eq!(
        linked_local.snapshot("HEAD").await.unwrap().commit_id(),
        fifth
    );

    client.close().await.unwrap();
    cleanup(&locator).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated writable S3 or RustFS bucket and built Git/Crab executables"]
async fn conflict_continue_and_abort_round_trip() {
    let scratch = tempfile::tempdir().unwrap();
    let (client, locator) = live_client(scratch.path());
    let initial = initial_commit(&client, &locator, scratch.path()).await;
    let remote_path = scratch.path().join("remote-writer");
    let merge_path = scratch.path().join("merge-conflict");
    let abort_path = scratch.path().join("rebase-abort");
    let remote_repository = client
        .clone_local(locator.clone(), &remote_path, CloneOptions::default())
        .await
        .unwrap();
    let merge_repository = client
        .clone_local(locator.clone(), &merge_path, CloneOptions::default())
        .await
        .unwrap();
    let abort_repository = client
        .clone_local(locator.clone(), &abort_path, CloneOptions::default())
        .await
        .unwrap();
    let remote = remote_repository.local().unwrap();
    let merge = merge_repository.local().unwrap();
    let abort = abort_repository.local().unwrap();

    std::fs::write(merge_path.join("README.md"), b"local merge change\n").unwrap();
    merge.stage(vec!["README.md".into()]).await.unwrap();
    let identity = self::identity();
    merge
        .commit(
            LocalCommitOptions::new(identity.clone(), identity, b"local merge change\n".to_vec())
                .unwrap(),
        )
        .await
        .unwrap();
    std::fs::write(abort_path.join("README.md"), b"local rebase change\n").unwrap();
    abort.stage(vec!["README.md".into()]).await.unwrap();
    let identity = self::identity();
    let abort_head = abort
        .commit(
            LocalCommitOptions::new(
                identity.clone(),
                identity,
                b"local rebase change\n".to_vec(),
            )
            .unwrap(),
        )
        .await
        .unwrap();

    std::fs::write(remote_path.join("README.md"), b"remote change\n").unwrap();
    remote.stage(vec!["README.md".into()]).await.unwrap();
    let identity = self::identity();
    remote
        .commit(
            LocalCommitOptions::new(identity.clone(), identity, b"remote change\n".to_vec())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        remote
            .prepare_push(PushOptions::current_branch())
            .await
            .unwrap()
            .execute()
            .await
            .unwrap(),
        LocalPushOutcome::Committed { .. }
    ));

    let PullOutcome::Conflict(conflict) = merge
        .pull(PullOptions::merge().hydrate(false))
        .await
        .unwrap()
    else {
        panic!("divergent merge did not conflict")
    };
    assert_eq!(conflict.kind(), IntegrationKind::Merge);
    assert_eq!(conflict.paths(), &[PathBuf::from("README.md")]);
    std::fs::write(merge_path.join("README.md"), b"resolved merge\n").unwrap();
    merge.stage(vec!["README.md".into()]).await.unwrap();
    assert!(matches!(
        merge
            .continue_integration(conflict.id().clone())
            .await
            .unwrap(),
        PullOutcome::Updated { .. }
    ));
    let git = PathBuf::from(required("CRAB_SDK_TEST_GIT_BIN"));
    let merge_parents = std::process::Command::new(&git)
        .current_dir(&merge_path)
        .args(["rev-list", "--parents", "-n", "1", "HEAD"])
        .output()
        .unwrap();
    assert!(merge_parents.status.success());
    assert_eq!(
        String::from_utf8(merge_parents.stdout)
            .unwrap()
            .split_whitespace()
            .count(),
        3
    );

    let PullOutcome::Conflict(conflict) = abort
        .pull(PullOptions::rebase().hydrate(false))
        .await
        .unwrap()
    else {
        panic!("divergent rebase did not conflict")
    };
    assert_eq!(conflict.kind(), IntegrationKind::Rebase);
    std::fs::write(abort_path.join("later.txt"), b"preserve after abort\n").unwrap();
    assert_eq!(
        abort
            .abort_integration(conflict.id().clone())
            .await
            .unwrap(),
        abort_head
    );
    assert_eq!(
        std::fs::read(abort_path.join("later.txt")).unwrap(),
        b"preserve after abort\n"
    );
    assert_eq!(
        abort.snapshot("HEAD").await.unwrap().commit_id(),
        abort_head
    );
    assert_ne!(abort_head, initial);

    client.close().await.unwrap();
    cleanup(&locator).await;
}
