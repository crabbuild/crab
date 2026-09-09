use std::{io::BufReader, path::Path, sync::Arc};

use super::*;
use crate::{GitPath, ObjectId, RefUpdate, Revision};

fn prepared_or_panic(result: crate::Result<PreparedMutation>) -> PreparedMutation {
    result.unwrap_or_else(|error| {
        let mut message = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(error) = source {
            message.push_str(&format!("\ncaused by: {error}"));
            source = error.source();
        }
        panic!("{message}");
    })
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .args([
            "-c",
            "user.name=SDK write fixture",
            "-c",
            "user.email=sdk@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

async fn fixture(
    client: &Client,
    locator: &RepositoryLocator,
    scratch: &Path,
) -> (ObjectId, ObjectId) {
    let source = tempfile::tempdir_in(scratch).unwrap();
    git(
        source.path(),
        &["init", "--initial-branch=main", "--object-format=sha1"],
    );
    let mut commits = Vec::new();
    for content in ["first\n", "second\n"] {
        std::fs::write(source.path().join("README.md"), content).unwrap();
        git(source.path(), &["add", "README.md"]);
        git(source.path(), &["commit", "-m", "fixture"]);
        commits.push(
            ObjectId::from_hex(
                String::from_utf8(git(source.path(), &["rev-parse", "HEAD"]))
                    .unwrap()
                    .trim(),
            )
            .unwrap(),
        );
    }
    let pack = git(source.path(), &["pack-objects", "--stdout", "--all"]);
    // Subsequent preparation and SDK operations cannot use the oracle checkout.
    source.close().unwrap();
    let directory = tempfile::tempdir_in(scratch).unwrap();
    let path = directory.path().join("incoming");
    std::fs::write(&path, pack).unwrap();
    let layout = crab_storage::StoreLayout::new(
        client.0.store.clone(),
        locator.direct_prefix().unwrap().to_owned(),
    );
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let updates = vec![
        RefUpdate::create("refs/heads/main", commits[1])
            .unwrap()
            .owner()
            .unwrap(),
        RefUpdate::create("refs/heads/old", commits[0])
            .unwrap()
            .owner()
            .unwrap(),
    ];
    let names = updates
        .iter()
        .map(|update| update.name.clone())
        .collect::<Vec<_>>();
    let ready = publication::with_leases(
        &client.0.store,
        &layout,
        names,
        LEASE_TTL,
        &cancel,
        |holders, cancel| {
            let layout = &layout;
            async move {
                let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
                    &client.0.store,
                    layout,
                )
                .await?;
                let repository = open(
                    client,
                    layout,
                    locator,
                    &OperationOptions::default(),
                    &cancel,
                )
                .await?;
                let prepared = crab_remote::prepare::prepare(
                    repository,
                    directory.path().to_owned(),
                    Some(BufReader::new(std::fs::File::open(path)?)),
                    updates,
                    BTreeMap::new(),
                    &cancel,
                    crab_remote::prepare::Options {
                        layout: layout.clone(),
                        graph: crab_git::receive_plan::GraphLimits {
                            max_ref_updates: 16,
                            max_graph_steps: 1024,
                            max_object_bytes: 1024 * 1024,
                            max_read_bytes: 4 * 1024 * 1024,
                        },
                        pack: crab_git::incoming_pack::ReceiveLimits {
                            max_pack_bytes: 4 * 1024 * 1024,
                            max_objects: 1024,
                            max_object_bytes: 1024 * 1024,
                            max_inflated_bytes: 4 * 1024 * 1024,
                            max_delta_depth: 128,
                        },
                        policy: |_: &str| crab_git::receive_plan::RefPolicy {
                            allow_delete: true,
                            allow_non_fast_forward: false,
                        },
                    },
                )
                .await?;
                let artifacts = prepared
                    .upload(
                        &snapshot,
                        dependency_limits(Default::default()),
                        &holders,
                        &cancel,
                    )
                    .await?;
                assert!(matches!(
                    artifacts
                        .commit(
                            None,
                            crab_write::journal::CommitOptions::new(LEASE_TTL, &cancel)
                        )
                        .await?,
                    CommitOutcome::Committed(_)
                ));
                Ok::<_, Failure>(
                    readiness(
                        client,
                        layout,
                        locator,
                        &OperationOptions::default(),
                        &cancel,
                    )
                    .await,
                )
            }
        },
    )
    .await
    .unwrap();
    assert!(matches!(ready, Readiness::Ready { .. }));
    (commits[0], commits[1])
}

pub(super) fn memory_client() -> Client {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_target_identity([42; 32])
        .with_bucket_identity(crab_storage::BucketIdentity::new(
            crab_storage::StorageProviderKind::S3,
            "memory-test",
            "bucket",
        ));
    Client(Arc::new(super::super::ClientState {
        store,
        namespace: "memory-test".to_owned(),
        #[cfg(feature = "content")]
        cache: None,
        operations: crate::runtime::Operations::default(),
        git: Arc::new(crab_remote_git::RemoteGitRuntime::default()),
        direct_store_configured: true,
        #[cfg(feature = "local")]
        local_store: None,
        #[cfg(feature = "local")]
        local_tools: None,
        #[cfg(feature = "local")]
        local_execution_policy: crate::LocalExecutionPolicy::Untrusted,
        #[cfg(feature = "managed")]
        managed: None,
    }))
}

#[test]
fn recovery_downloads_are_admitted_as_one_aggregate_budget() {
    let content = crate::mutation::ContentBinding {
        xorbs: vec![crate::mutation::ContentArtifactBinding {
            protocol_hash: [1; 32],
            body_hash: [2; 32],
            size: 6,
        }],
        shards: vec![crate::mutation::ContentArtifactBinding {
            protocol_hash: [3; 32],
            body_hash: [4; 32],
            size: 5,
        }],
        files: vec![crate::mutation::ContentFileBinding {
            file_hash: [5; 32],
            size: 7,
            shard_hash: [3; 32],
        }],
    };
    let input = PackInput::Staged {
        pack_id: "a".repeat(64),
        size: 2,
        plan_id: "b".repeat(64),
    };
    let limits = crate::ReadLimits {
        max_fetched_bytes: 12,
        ..Default::default()
    };

    let error = validate_recovery_budget(&input, Some(&content), limits).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::LimitExceeded);
}

#[tokio::test]
async fn reconciliation_deadline_drains_a_delayed_receipt_read() {
    let mut client = memory_client();
    let store = client.0.store.clone();
    let delayed = object_store::throttle::ThrottledStore::new(
        store.inner().clone(),
        object_store::throttle::ThrottleConfig {
            wait_get_per_call: Duration::from_secs(60),
            ..Default::default()
        },
    );
    Arc::get_mut(&mut client.0).unwrap().store = crab_storage::Store::new(Arc::new(delayed))
        .with_target_identity(*store.target_identity().unwrap());
    let token = RecoveryToken::new(
        *store.target_identity().unwrap(),
        &RepositoryLocator::new("repository").unwrap(),
        RefBatch::new(vec![
            RefUpdate::create("refs/tags/v1", ObjectId::from_hex(&"a".repeat(40)).unwrap())
                .unwrap(),
        ])
        .unwrap(),
        None,
        None,
        None,
    )
    .unwrap();
    let options = OperationOptions::default()
        .with_timeout(Duration::from_millis(10))
        .unwrap();
    let result =
        tokio::time::timeout(Duration::from_secs(1), client.reconcile(token, options)).await;
    let closed = tokio::time::timeout(Duration::from_secs(1), client.close()).await;
    assert!(matches!(result, Ok(Err(error)) if error.kind() == ErrorKind::Timeout));
    assert!(matches!(closed, Ok(Ok(()))));
}

#[tokio::test(flavor = "multi_thread")]
async fn atomic_ref_edits_preserve_recovery_after_refs_change() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (first, second) = fixture(&client, &locator, scratch.path()).await;
    let repo = client.open_remote(locator.clone()).await.unwrap();
    let batch = RefBatch::new(vec![RefUpdate::create("refs/tags/v1", second).unwrap()]).unwrap();
    let prepared = repo
        .prepare_ref_update(
            batch,
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let token = RecoveryToken::from_json(&prepared.recovery_token().to_json().unwrap()).unwrap();
    assert!(matches!(
        client
            .reconcile(token.clone(), OperationOptions::default())
            .await
            .unwrap(),
        MutationOutcome::Indeterminate { .. }
    ));
    let MutationOutcome::Committed { receipt, readiness } =
        prepared.execute(OperationOptions::default()).await.unwrap()
    else {
        panic!("expected commitment")
    };
    assert!(matches!(readiness, Readiness::Ready { .. }));
    let transaction = receipt.transaction_id().to_owned();
    let changed = repo
        .prepare_ref_update(
            RefBatch::new(vec![
                RefUpdate::delete("refs/tags/v1", second).unwrap(),
                RefUpdate::update("refs/heads/main", second, first).unwrap(),
            ])
            .unwrap()
            .with_policy(WritePolicy::ForceWithLease),
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        changed.execute(OperationOptions::default()).await.unwrap(),
        MutationOutcome::Committed { .. }
    ));
    let MutationOutcome::Committed { receipt, .. } = client
        .reconcile(token.clone(), OperationOptions::default())
        .await
        .unwrap()
    else {
        panic!("historical proof was lost")
    };
    assert_eq!(receipt.transaction_id(), transaction);
    let current = client.open_remote(locator).await.unwrap();
    let content = current
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap()
        .read_blob(GitPath::new("README.md").unwrap())
        .await
        .unwrap();
    assert_eq!(content.as_ref(), b"first\n");
    client.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn competing_writer_rejects_the_whole_prepared_batch() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (first, second) = fixture(&client, &locator, scratch.path()).await;
    let repo = client.open_remote(locator.clone()).await.unwrap();
    let first_batch = RefBatch::new(vec![
        RefUpdate::create("refs/tags/stale", second).unwrap(),
        RefUpdate::update("refs/heads/main", second, first).unwrap(),
    ])
    .unwrap()
    .with_policy(WritePolicy::ForceWithLease);
    let prepared = repo
        .prepare_ref_update(
            first_batch,
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let competitor = repo
        .prepare_ref_update(
            RefBatch::new(vec![
                RefUpdate::update("refs/heads/main", second, first).unwrap(),
            ])
            .unwrap()
            .with_policy(WritePolicy::ForceWithLease),
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
    assert!(matches!(
        prepared.execute(OperationOptions::default()).await.unwrap(),
        MutationOutcome::Rejected { .. }
    ));
    let refs = client
        .open_remote(locator)
        .await
        .unwrap()
        .refs()
        .await
        .unwrap();
    assert!(
        !refs
            .entries()
            .iter()
            .any(|reference| reference.name() == "refs/tags/stale")
    );
    client.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn prepared_commit_retains_its_pack_and_executes_directly() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (_, base) = fixture(&client, &locator, scratch.path()).await;
    let repo = client.open_remote(locator.clone()).await.unwrap();
    let identity =
        crate::CommitIdentity::new("SDK author", "sdk@example.invalid", 1_700_000_000, 0).unwrap();
    let content = b"direct execution\n".to_vec();
    let prepared = repo
        .prepare_commit(
            crate::CommitOptions::new(
                base,
                "refs/heads/main",
                Some(base),
                identity.clone(),
                identity,
                b"direct commit\n".to_vec(),
            )
            .unwrap(),
            vec![
                crate::FileEdit::git(
                    GitPath::new("README.md").unwrap(),
                    crate::EntryMode::Regular,
                    content.len() as u64,
                    std::io::Cursor::new(content.clone()),
                )
                .unwrap(),
            ],
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let commit = prepared.commit_id().unwrap();
    assert!(
        prepared
            .directories
            .iter()
            .all(|directory| directory.path().exists())
    );
    assert!(matches!(
        prepared.execute(OperationOptions::default()).await.unwrap(),
        MutationOutcome::Committed {
            readiness: Readiness::Ready { .. },
            ..
        }
    ));
    let snapshot = client
        .open_remote(locator)
        .await
        .unwrap()
        .snapshot(Revision::commit(commit))
        .await
        .unwrap();
    assert_eq!(
        snapshot
            .read_blob(GitPath::new("README.md").unwrap())
            .await
            .unwrap()
            .as_ref(),
        content
    );
    client.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_commit_bootstraps_an_initialized_repository() {
    let client = memory_client();
    let locator = RepositoryLocator::new("empty").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap();
    let repo = client.open_remote(locator.clone()).await.unwrap();
    let identity =
        crate::CommitIdentity::new("SDK author", "sdk@example.invalid", 1_700_000_000, 0).unwrap();
    let content = b"first content\n".to_vec();
    let prepared = repo
        .prepare_commit(
            crate::CommitOptions::initial(
                "refs/heads/main",
                identity.clone(),
                identity,
                b"initial commit\n".to_vec(),
            )
            .unwrap(),
            vec![
                crate::FileEdit::git(
                    GitPath::new("README.md").unwrap(),
                    crate::EntryMode::Regular,
                    content.len() as u64,
                    std::io::Cursor::new(content.clone()),
                )
                .unwrap(),
            ],
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let commit = prepared.commit_id().unwrap();
    assert!(matches!(
        prepared.execute(OperationOptions::default()).await.unwrap(),
        MutationOutcome::Committed { .. }
    ));
    let snapshot = client
        .open_remote(locator)
        .await
        .unwrap()
        .snapshot(Revision::commit(commit))
        .await
        .unwrap();
    assert!(snapshot.commit().await.unwrap().parents.is_empty());
    assert_eq!(
        snapshot
            .read_blob(GitPath::new("README.md").unwrap())
            .await
            .unwrap()
            .as_ref(),
        content
    );
    client.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_commit_rejects_mis_sized_streams_before_publication() {
    for (declared, bytes) in [(2, b"x".as_slice()), (1, b"xx".as_slice())] {
        let client = memory_client();
        let locator = RepositoryLocator::new("repository").unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let (_, base) = fixture(&client, &locator, scratch.path()).await;
        let repo = client.open_remote(locator.clone()).await.unwrap();
        let identity =
            crate::CommitIdentity::new("SDK author", "sdk@example.invalid", 1, 0).unwrap();
        let result = repo
            .prepare_commit(
                crate::CommitOptions::new(
                    base,
                    "refs/heads/main",
                    Some(base),
                    identity.clone(),
                    identity,
                    b"invalid stream".to_vec(),
                )
                .unwrap(),
                vec![
                    crate::FileEdit::git(
                        GitPath::new("new.txt").unwrap(),
                        crate::EntryMode::Regular,
                        declared,
                        std::io::Cursor::new(bytes.to_vec()),
                    )
                    .unwrap(),
                ],
                scratch.path().to_owned(),
                OperationOptions::default(),
            )
            .await;
        let Err(error) = result else {
            panic!("mis-sized stream was accepted")
        };
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let refs = client
            .open_remote(locator)
            .await
            .unwrap()
            .refs()
            .await
            .unwrap();
        assert_eq!(
            refs.entries()
                .iter()
                .find(|entry| entry.name() == "refs/heads/main")
                .unwrap()
                .target(),
            base
        );
        client.close().await.unwrap();
    }
}

#[cfg(feature = "content")]
#[tokio::test(flavor = "multi_thread")]
async fn remote_commit_streams_git_and_hydrated_content_and_resumes_from_its_pack() {
    let mut client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let cache = scratch.path().join("content-cache");
    std::fs::create_dir(&cache).unwrap();
    Arc::get_mut(&mut client.0).unwrap().cache =
        Some(crate::ContentCache::new(&cache, 4 * 1024 * 1024).unwrap());
    let (_, base) = fixture(&client, &locator, scratch.path()).await;
    let repo = client.open_remote(locator.clone()).await.unwrap();
    let identity =
        crate::CommitIdentity::new("SDK author", "sdk@example.invalid", 1_700_000_000, -420)
            .unwrap();
    let text = b"created without a checkout\n".to_vec();
    let large = vec![b'x'; 2 * 1024 * 1024];
    let prepared = prepared_or_panic(
        repo.prepare_commit(
            crate::CommitOptions::new(
                base,
                "refs/heads/main",
                Some(base),
                identity.clone(),
                identity,
                b"remote edit\n".to_vec(),
            )
            .unwrap(),
            vec![
                crate::FileEdit::git(
                    GitPath::new("README.md").unwrap(),
                    crate::EntryMode::Regular,
                    text.len() as u64,
                    std::io::Cursor::new(text.clone()),
                )
                .unwrap(),
                crate::FileEdit::hydrated(
                    GitPath::new("large.bin").unwrap(),
                    crate::EntryMode::Executable,
                    large.len() as u64,
                    std::io::Cursor::new(large.clone()),
                )
                .unwrap(),
                crate::FileEdit::hydrated(
                    GitPath::new("empty.bin").unwrap(),
                    crate::EntryMode::Regular,
                    0,
                    std::io::Cursor::new(Vec::<u8>::new()),
                )
                .unwrap(),
            ],
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await,
    );
    let commit = prepared.commit_id().unwrap();
    let token = RecoveryToken::from_json(&prepared.recovery_token().to_json().unwrap()).unwrap();
    drop(prepared);
    assert!(matches!(
        client
            .resume_mutation(
                token,
                scratch.path().to_owned(),
                OperationOptions::default()
            )
            .await
            .unwrap(),
        MutationOutcome::Committed {
            readiness: Readiness::Ready { .. },
            ..
        }
    ));
    let current = client.open_remote(locator).await.unwrap();
    let snapshot = current.snapshot(Revision::commit(commit)).await.unwrap();
    let actual_text = snapshot
        .read_blob(GitPath::new("README.md").unwrap())
        .await
        .unwrap();
    assert_eq!(actual_text.as_ref(), text);
    let raw_pointer = snapshot
        .read_blob(GitPath::new("large.bin").unwrap())
        .await
        .unwrap();
    assert!(raw_pointer.starts_with(b"version https://crab.build/spec/v1\n"));
    let mut stream = snapshot
        .open_file(GitPath::new("large.bin").unwrap())
        .await
        .unwrap();
    let mut hydrated = Vec::new();
    while let Some(chunk) = stream.next().await.unwrap() {
        hydrated.extend_from_slice(&chunk);
    }
    assert_eq!(hydrated, large);
    let mut empty = snapshot
        .open_file(GitPath::new("empty.bin").unwrap())
        .await
        .unwrap();
    assert!(empty.next().await.unwrap().is_none());
    client.close().await.unwrap();
}

#[cfg(feature = "content")]
#[tokio::test(flavor = "multi_thread")]
async fn recovered_hydrated_content_rejects_a_changed_xorb_without_moving_refs() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (_, base) = fixture(&client, &locator, scratch.path()).await;
    let repo = client.open_remote(locator.clone()).await.unwrap();
    let identity = crate::CommitIdentity::new("SDK author", "sdk@example.invalid", 1, 0).unwrap();
    let prepared = prepared_or_panic(
        repo.prepare_commit(
            crate::CommitOptions::new(
                base,
                "refs/heads/main",
                Some(base),
                identity.clone(),
                identity,
                b"native content".to_vec(),
            )
            .unwrap(),
            vec![
                crate::FileEdit::hydrated(
                    GitPath::new("large.bin").unwrap(),
                    crate::EntryMode::Regular,
                    2 * 1024 * 1024,
                    std::io::Cursor::new(vec![b'x'; 2 * 1024 * 1024]),
                )
                .unwrap(),
            ],
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await,
    );
    let token = prepared.recovery_token().clone();
    let xorb = &token.content().unwrap().xorbs[0];
    let layout = crab_storage::StoreLayout::new(
        client.0.store.clone(),
        locator.direct_prefix().unwrap().to_owned(),
    );
    let path = layout.ref_journal_recovery_xorb_path(
        token.plan_id(),
        &crab_xet::hash::MerkleHash::from(xorb.protocol_hash),
    );
    client.0.store.delete(&path).await.unwrap();
    client
        .0
        .store
        .put_exact(&path, bytes::Bytes::from(vec![0; xorb.size as usize]))
        .await
        .unwrap();
    drop(prepared);

    let error = client
        .resume_mutation(
            token,
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Corruption);
    let refs = client
        .open_remote(locator)
        .await
        .unwrap()
        .refs()
        .await
        .unwrap();
    assert_eq!(
        refs.entries()
            .iter()
            .find(|entry| entry.name() == "refs/heads/main")
            .unwrap()
            .target(),
        base
    );
    client.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_never_replays_an_unresolved_prior_intent() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (_, tip) = fixture(&client, &locator, scratch.path()).await;
    let repo = client.open_remote(locator.clone()).await.unwrap();
    let prepared = repo
        .prepare_ref_update(
            RefBatch::new(vec![RefUpdate::create("refs/tags/uncertain", tip).unwrap()]).unwrap(),
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let token = prepared.recovery_token().clone();
    drop(prepared);
    let layout = crab_storage::StoreLayout::new(
        client.0.store.clone(),
        locator.direct_prefix().unwrap().to_owned(),
    );
    let intent = crab_metadata::plan_receipt::PlanIntent {
        version: 1,
        repo_prefix: locator.direct_prefix().unwrap().to_owned(),
        plan_id: token.plan_id().to_owned(),
        attempt: 1,
        commit: crab_metadata::plan_receipt::PlanCommit::RefJournal {
            transaction_id: "a".repeat(64),
            dependency_digest: "b".repeat(64),
        },
    };
    client
        .0
        .store
        .put(
            &layout.ref_journal_plan_intent_path(token.plan_id(), 1),
            bytes::Bytes::from(serde_json::to_vec(&intent).unwrap()),
        )
        .await
        .unwrap();
    let outcome = client
        .resume_mutation(
            token.clone(),
            scratch.path().join("absent"),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let refs = client
        .open_remote(locator)
        .await
        .unwrap()
        .refs()
        .await
        .unwrap();
    client.close().await.unwrap();
    assert!(
        matches!(outcome, MutationOutcome::Indeterminate { recovery } if recovery.plan_id() == token.plan_id())
    );
    assert!(
        !refs
            .entries()
            .iter()
            .any(|entry| entry.name() == "refs/tags/uncertain")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_ref_plan_resumes_after_preparation_is_dropped() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (_, tip) = fixture(&client, &locator, scratch.path()).await;
    let repo = client.open_remote(locator).await.unwrap();
    let prepared = repo
        .prepare_ref_update(
            RefBatch::new(vec![RefUpdate::create("refs/tags/resumed", tip).unwrap()]).unwrap(),
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let json = prepared.recovery_token().to_json().unwrap();
    let directory = prepared.directories[0].path().to_owned();
    drop(prepared);
    assert!(!directory.exists());
    let token = RecoveryToken::from_json(&json).unwrap();
    let first = client
        .resume_mutation(
            token.clone(),
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    // A committed plan must resolve before touching scratch or rebuilding its now-stale request.
    let second = client
        .resume_mutation(
            token,
            scratch.path().join("absent"),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    client.close().await.unwrap();
    let transaction = |outcome| match outcome {
        MutationOutcome::Committed { receipt, .. } => receipt.transaction_id().to_owned(),
        outcome => panic!("resume did not retain commitment: {outcome:?}"),
    };
    assert_eq!(transaction(first), transaction(second));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_execution_of_one_token_never_replays() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (_, tip) = fixture(&client, &locator, scratch.path()).await;
    let repo = client.open_remote(locator).await.unwrap();
    let batch = RefBatch::new(vec![
        RefUpdate::create("refs/tags/shared-plan", tip).unwrap(),
    ])
    .unwrap();
    let first = repo
        .prepare_ref_update(
            batch.clone(),
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let token = RecoveryToken::from_json(&first.recovery_token().to_json().unwrap()).unwrap();
    let (first, second) = tokio::join!(
        first.execute(OperationOptions::default()),
        client.resume_mutation(
            token.clone(),
            scratch.path().to_owned(),
            OperationOptions::default()
        ),
    );
    let mut transactions = Vec::new();
    for result in [first, second] {
        match result {
            Ok(MutationOutcome::Committed { receipt, .. }) => {
                transactions.push(receipt.transaction_id().to_owned());
            }
            Err(error) if error.kind() == ErrorKind::Conflict => {}
            result => panic!("unexpected same-plan result: {result:?}"),
        }
    }
    let recovered = client
        .reconcile(token, OperationOptions::default())
        .await
        .unwrap();
    client.close().await.unwrap();
    let MutationOutcome::Committed { receipt, .. } = recovered else {
        panic!("same-plan recovery lost commitment: {recovered:?}");
    };
    assert!(
        !transactions.is_empty() && transactions.iter().all(|id| id == receipt.transaction_id())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn readiness_respects_selected_read_limits_without_losing_commitment() {
    for during_execution in [true, false] {
        let client = memory_client();
        let locator = RepositoryLocator::new("repository").unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let (_, tip) = fixture(&client, &locator, scratch.path()).await;
        let repo = client.open_remote(locator).await.unwrap();
        let prepared = repo
            .prepare_ref_update(
                RefBatch::new(vec![RefUpdate::create("refs/tags/budget", tip).unwrap()]).unwrap(),
                scratch.path().to_owned(),
                OperationOptions::default(),
            )
            .await
            .unwrap();
        let token = prepared.recovery_token().clone();
        let limited = OperationOptions::default()
            .with_limits(crate::ReadLimits {
                max_storage_requests: 1,
                ..Default::default()
            })
            .unwrap();
        let outcome = if during_execution {
            prepared.execute(limited).await.unwrap()
        } else {
            assert!(matches!(
                prepared.execute(OperationOptions::default()).await.unwrap(),
                MutationOutcome::Committed {
                    readiness: Readiness::Ready { .. },
                    ..
                }
            ));
            client.reconcile(token.clone(), limited).await.unwrap()
        };
        let MutationOutcome::Committed { receipt, readiness } = outcome else {
            panic!("readiness exhaustion must retain commitment");
        };
        assert_eq!(
            readiness,
            Readiness::Pending,
            "execution={during_execution}"
        );
        let recovered = client
            .reconcile(token, OperationOptions::default())
            .await
            .unwrap();
        assert!(
            matches!(recovered, MutationOutcome::Committed { receipt: recovered, readiness: Readiness::Ready { .. } } if recovered.transaction_id() == receipt.transaction_id())
        );
        client.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rewind_requires_explicit_force_with_lease() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (first, second) = fixture(&client, &locator, scratch.path()).await;
    let repo = client.open_remote(locator.clone()).await.unwrap();
    let batch = RefBatch::new(vec![
        RefUpdate::update("refs/heads/main", second, first).unwrap(),
    ])
    .unwrap();
    let error = repo
        .prepare_ref_update(
            batch,
            scratch.path().to_owned(),
            OperationOptions::default(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    let refs = client
        .open_remote(locator)
        .await
        .unwrap()
        .refs()
        .await
        .unwrap();
    assert_eq!(
        refs.entries()
            .iter()
            .find(|reference| reference.name() == "refs/heads/main")
            .unwrap()
            .target(),
        second
    );
    client.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated RustFS prefix and environment credentials"]
async fn ref_batches_rustfs_recover_without_git_after_restart() {
    const PHASE: &str = "CRAB_SDK_REF_TEST_PHASE";
    const SCRATCH: &str = "CRAB_SDK_REF_TEST_SCRATCH";
    let bucket = std::env::var("QUALIFICATION_BUCKET").unwrap();
    let prefix = std::env::var("QUALIFICATION_PREFIX").unwrap();
    assert!(prefix.starts_with("qualification/sdk-ref-"));
    let locator = RepositoryLocator::new(&prefix).unwrap();
    let client = Client::builder()
        .direct_store(crate::DirectStoreOptions::s3_from_env(&bucket).unwrap())
        .build()
        .unwrap();
    if let Ok(phase) = std::env::var(PHASE) {
        let scratch = PathBuf::from(std::env::var_os(SCRATCH).unwrap());
        let saved = scratch.join("recovery.json");
        if phase == "initialize" {
            client
                .initialize_remote(locator.clone(), "refs/heads/main")
                .await
                .unwrap();
            let refs = client
                .open_remote(locator)
                .await
                .unwrap()
                .refs()
                .await
                .unwrap();
            assert_eq!(refs.head(), Some("refs/heads/main"));
            assert!(refs.entries().is_empty());
        } else if phase == "prepare" {
            let repo = client.open_remote(locator.clone()).await.unwrap();
            let tip = repo
                .refs()
                .await
                .unwrap()
                .entries()
                .iter()
                .find(|reference| reference.name() == "refs/heads/main")
                .unwrap()
                .target();
            let prepared = repo
                .prepare_ref_update(
                    RefBatch::new(vec![RefUpdate::create("refs/tags/recover", tip).unwrap()])
                        .unwrap(),
                    scratch.clone(),
                    OperationOptions::default(),
                )
                .await
                .unwrap();
            // Persist before crossing the journal marker; a new process gets only this token.
            std::fs::write(&saved, prepared.recovery_token().to_json().unwrap()).unwrap();
            std::fs::File::open(&saved).unwrap().sync_all().unwrap();
            drop(prepared);
        } else if phase == "execute" {
            let token =
                RecoveryToken::from_json(&std::fs::read_to_string(&saved).unwrap()).unwrap();
            let MutationOutcome::Committed { receipt, readiness } = client
                .resume_mutation(token, scratch.clone(), OperationOptions::default())
                .await
                .unwrap()
            else {
                panic!("expected direct commitment")
            };
            assert_eq!(readiness, Readiness::Pending);
            std::fs::write(scratch.join("transaction"), receipt.transaction_id()).unwrap();
        } else if phase == "advance" {
            let repo = client.open_remote(locator.clone()).await.unwrap();
            let tip = repo
                .refs()
                .await
                .unwrap()
                .entries()
                .iter()
                .find(|reference| reference.name() == "refs/tags/recover")
                .unwrap()
                .target();
            let prepared = repo
                .prepare_ref_update(
                    RefBatch::new(vec![RefUpdate::delete("refs/tags/recover", tip).unwrap()])
                        .unwrap(),
                    scratch.clone(),
                    OperationOptions::default(),
                )
                .await
                .unwrap();
            assert!(matches!(
                prepared.execute(OperationOptions::default()).await.unwrap(),
                MutationOutcome::Committed {
                    readiness: Readiness::Ready { .. },
                    ..
                }
            ));
        } else {
            if phase == "pending" {
                assert_eq!(
                    client
                        .open_remote(locator.clone())
                        .await
                        .err()
                        .unwrap()
                        .kind(),
                    ErrorKind::Indexing
                );
            } else {
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
                        .any(|reference| reference.name() == "refs/tags/recover")
                );
            }
            let token = RecoveryToken::from_json(&std::fs::read_to_string(saved).unwrap()).unwrap();
            if phase == "pending" {
                assert!(matches!(
                    client
                        .resume_mutation(
                            token.clone(),
                            scratch.join("absent"),
                            OperationOptions::default()
                        )
                        .await
                        .unwrap(),
                    MutationOutcome::Committed {
                        readiness: Readiness::Pending,
                        ..
                    }
                ));
            }
            let MutationOutcome::Committed { receipt, readiness } = client
                .reconcile(token, OperationOptions::default())
                .await
                .unwrap()
            else {
                panic!("saved token must prove the historical commit")
            };
            assert_eq!(
                receipt.transaction_id(),
                std::fs::read_to_string(scratch.join("transaction")).unwrap()
            );
            if phase == "pending" {
                assert_eq!(readiness, Readiness::Pending);
            }
        }
        client.close().await.unwrap();
        return;
    }

    let scratch = tempfile::tempdir().unwrap();
    let no_tools = scratch.path().join("empty-path");
    std::fs::create_dir(&no_tools).unwrap();
    let run = |phase: &str| {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "client::mutation::tests::ref_batches_rustfs_recover_without_git_after_restart",
                "--ignored",
                "--nocapture",
            ])
            .env(PHASE, phase)
            .env(SCRATCH, scratch.path())
            .env("PATH", &no_tools)
            .env("GIT_EXEC_PATH", &no_tools)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "phase {phase}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    run("initialize");
    fixture(&client, &locator, scratch.path()).await;
    let layout = crab_storage::StoreLayout::new(client.0.store.clone(), prefix);
    let owner = crab_coordination::PushLock::acquire_internal(
        client.0.store.inner(),
        layout.repo_prefix(),
        crab_coordination::GIT_GENERATION_OWNER_RESOURCE,
        Duration::from_secs(300),
    )
    .await
    .unwrap();
    run("prepare");
    run("execute");
    run("pending");
    owner.release().await.unwrap();
    let cancel = CancellationToken::new();
    let ready = publication::with_leases(
        &client.0.store,
        &layout,
        Vec::<String>::new(),
        LEASE_TTL,
        &cancel,
        |_, cancel| {
            let (client, layout, locator) = (&client, &layout, &locator);
            async move {
                Ok::<_, Failure>(
                    readiness(
                        client,
                        layout,
                        locator,
                        &OperationOptions::default(),
                        &cancel,
                    )
                    .await,
                )
            }
        },
    )
    .await
    .unwrap();
    assert!(matches!(ready, Readiness::Ready { .. }));
    run("advance");
    run("historical");
    client.close().await.unwrap();
}
