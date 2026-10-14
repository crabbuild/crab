use super::*;

async fn snapshot_with_read_indexes(
    fixture: &OpenFixture,
    paged: bool,
) -> crab_metadata::manifest_store::RepositorySnapshot {
    use crab_metadata::path_state::{
        PathStateInput, PathStateMutation, append_path_state, upload_path_state,
    };
    use crab_metadata::split_commit_graph::{
        CommitGraphInput, append_paged_commit_graph, append_split_commit_graph,
        load_split_commit_graph, upload_split_commit_graph,
    };

    crab_metadata::layout_descriptor::ensure_canonical_layout(&fixture.store, &fixture.layout)
        .await
        .unwrap();
    let mut snapshot =
        crab_metadata::manifest_store::read_repository_snapshot(&fixture.store, &fixture.layout)
            .await
            .unwrap();
    let manifest = &mut snapshot.manifest;
    let append = if paged {
        append_paged_commit_graph
    } else {
        append_split_commit_graph
    };
    let graph = append(
        None,
        manifest.generation,
        manifest.pack_index_hash.clone(),
        manifest.git_validation_digest.clone(),
        &[[0x11; 20]],
        vec![CommitGraphInput {
            oid: [0x11; 20],
            tree_oid: [0x22; 20],
            commit_time: 123,
            parents: Vec::new(),
        }],
    )
    .unwrap()
    .unwrap();
    upload_split_commit_graph(&fixture.store, &fixture.layout, &graph)
        .await
        .unwrap();
    manifest.commit_graph_hash = Some(graph.descriptor_hash.clone());
    let graph = load_split_commit_graph(
        &fixture.store,
        &fixture.layout,
        &graph.descriptor_hash,
        ObjectLimits::default().max_commit_graph_bytes,
    )
    .await
    .unwrap();
    let paths = append_path_state(
        None,
        &graph,
        vec![PathStateInput {
            oid: [0x11; 20],
            first_parent: None,
            author: b"Author".to_vec(),
            author_seconds: 123,
            message: b"initial".to_vec(),
            mutations: vec![PathStateMutation {
                path: b"README.md".to_vec(),
                present: true,
                reset: false,
            }],
        }],
    )
    .unwrap();
    upload_path_state(
        &fixture.store,
        &fixture.layout,
        &paths,
        crab_metadata::path_state::DEFAULT_MAX_PATH_STATE_BYTES,
    )
    .await
    .unwrap();
    manifest.path_state_hash = Some(paths.descriptor_hash);
    snapshot
}

#[tokio::test]
async fn snapshot_read_indexes_supply_exact_attribution_without_manifest_or_catalog() {
    let fixture = open_fixture(1, None).await;
    let snapshot = snapshot_with_read_indexes(&fixture, true).await;
    // Full maintenance reads are valid during construction. Foreground opening
    // and attribution must succeed when only graph-page ranges are admitted.
    fixture
        .backend
        .reject_graph_layer_bodies
        .store(true, Ordering::SeqCst);
    fixture
        .store
        .delete(&fixture.layout.manifest_path())
        .await
        .unwrap();
    let runtime = Arc::new(RemoteGitRuntime::default());
    for mode in 0..3 {
        let identity = RepositoryIdentity::new("memory", "org/repo", 1).unwrap();
        let cancel = CancellationToken::new();
        let repository = match mode {
            0 => {
                RemoteGitRepository::from_snapshot(
                    fixture.layout.clone(),
                    &snapshot,
                    identity,
                    runtime.clone(),
                    RepositoryOptions::default(),
                    &cancel,
                )
                .await
            }
            1 => {
                RemoteGitRepository::from_snapshot_with_lookup_sources(
                    fixture.layout.clone(),
                    &snapshot,
                    identity,
                    runtime.clone(),
                    RepositoryOptions::default(),
                    SnapshotLookupSources::default(),
                    &cancel,
                )
                .await
            }
            _ => {
                RemoteGitRepository::from_snapshot_with_catalog_tail(
                    fixture.layout.clone(),
                    &snapshot,
                    identity,
                    runtime.clone(),
                    RepositoryOptions::default(),
                    &cancel,
                )
                .await
            }
        }
        .unwrap();
        assert!(repository.commit_graph_available());
        assert!(repository.path_state_available());
        let operation = repository
            .operation(crate::OperationKind::History, &cancel)
            .await
            .unwrap();
        let index = repository
            .state
            .path_state(&operation)
            .await
            .unwrap()
            .unwrap();
        let latest = index
            .latest(
                &repository.state.store,
                ObjectId::Sha1([0x11; 20]),
                &[crate::GitPath::new(b"README.md".to_vec()).unwrap()],
                &operation,
            )
            .await
            .unwrap();
        assert_eq!(latest[0].oid, ObjectId::Sha1([0x11; 20]));
        assert_eq!(latest[0].message.as_ref(), b"initial");
        operation.finish(Ok(())).await.unwrap();
    }
    runtime.shutdown().await;
}

#[tokio::test]
async fn snapshot_read_indexes_skip_origin_when_absent_or_superseded() {
    let fixture = open_fixture(1, None).await;
    let indexed = snapshot_with_read_indexes(&fixture, true).await;
    let runtime = Arc::new(RemoteGitRuntime::default());
    for absent in [false, true] {
        let mut snapshot = indexed.clone();
        if absent {
            snapshot.manifest.commit_graph_hash = None;
            snapshot.manifest.path_state_hash = None;
        } else {
            snapshot
                .journal
                .refs
                .insert("refs/heads/main".into(), "33".repeat(20));
        }
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let store = fixture
            .store
            .clone()
            .with_read_request_observer(Arc::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
            }));
        let repository = RemoteGitRepository::from_snapshot(
            StoreLayout::new(store, "org/repo".into()),
            &snapshot,
            RepositoryIdentity::new("memory", "org/repo", 1).unwrap(),
            runtime.clone(),
            RepositoryOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!repository.commit_graph_available());
        assert!(!repository.path_state_available());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }
    runtime.shutdown().await;
}

#[tokio::test]
async fn snapshot_read_indexes_do_not_hide_exhausted_request_or_byte_budgets() {
    let fixture = open_fixture(1, None).await;
    let snapshot = snapshot_with_read_indexes(&fixture, true).await;
    for limits in [
        OperationLimits {
            max_storage_requests: 1,
            ..Default::default()
        },
        OperationLimits {
            max_fetched_bytes: 1,
            ..Default::default()
        },
    ] {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let result = RemoteGitRepository::from_snapshot(
            fixture.layout.clone(),
            &snapshot,
            RepositoryIdentity::new("memory", "org/repo", 1).unwrap(),
            runtime.clone(),
            RepositoryOptions::new(ObjectLimits::default(), limits).unwrap(),
            &CancellationToken::new(),
        )
        .await;
        assert!(admission_rejected(&result.unwrap_err()));
        runtime.shutdown().await;
    }
}

#[tokio::test]
async fn snapshot_read_indexes_fail_attribution_when_declared_metadata_is_corrupt() {
    for kind in ["commit-graph", "path-state"] {
        let fixture = open_fixture(1, None).await;
        let snapshot = snapshot_with_read_indexes(&fixture, true).await;
        let hash = match kind {
            "commit-graph" => snapshot.manifest.commit_graph_hash.as_deref().unwrap(),
            _ => snapshot.manifest.path_state_hash.as_deref().unwrap(),
        };
        // Bypass immutable-write protection only to inject corrupt origin bytes.
        fixture
            .backend
            .inner
            .put(
                &fixture.layout.bulk_manifest_path(kind, hash),
                Bytes::from_static(b"corrupt").into(),
            )
            .await
            .unwrap();
        let runtime = Arc::new(RemoteGitRuntime::default());
        let cancel = CancellationToken::new();
        let repository = RemoteGitRepository::from_snapshot(
            fixture.layout.clone(),
            &snapshot,
            RepositoryIdentity::new("memory", "org/repo", 1).unwrap(),
            runtime.clone(),
            RepositoryOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        let operation = repository
            .operation(crate::OperationKind::History, &cancel)
            .await
            .unwrap();
        let result = repository.state.path_state(&operation).await.map(|_| ());
        assert!(matches!(
            operation.finish(result).await,
            Err(Error::Corrupt {
                stage: crate::CorruptionStage::PathState
            })
        ));
        runtime.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_read_indexes_drain_on_cancellation_shutdown_and_deadline() {
    for mode in ["caller", "shutdown", "deadline"] {
        let fixture = open_fixture(1, None).await;
        let snapshot = snapshot_with_read_indexes(&fixture, true).await;
        fixture
            .backend
            .block_path_containing("manifests/commit-graph-");
        let runtime = Arc::new(RemoteGitRuntime::default());
        let worker_runtime = runtime.clone();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let layout = fixture.layout.clone();
        let limits = OperationLimits {
            max_duration: if mode == "deadline" {
                Duration::from_millis(50)
            } else {
                Duration::from_secs(60)
            },
            ..Default::default()
        };
        let worker = tokio::spawn(async move {
            RemoteGitRepository::from_snapshot(
                layout,
                &snapshot,
                RepositoryIdentity::new("memory", "org/repo", 1).unwrap(),
                worker_runtime,
                RepositoryOptions::new(ObjectLimits::default(), limits).unwrap(),
                &worker_cancel,
            )
            .await
        });
        tokio::time::timeout(
            Duration::from_secs(2),
            fixture.backend.request_started.notified(),
        )
        .await
        .unwrap();
        if mode == "caller" {
            cancel.cancel();
        } else if mode == "shutdown" {
            tokio::time::timeout(Duration::from_secs(2), runtime.shutdown())
                .await
                .unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(
            match mode {
                "deadline" => matches!(result, Err(Error::Timeout { .. })),
                _ => matches!(result, Err(Error::Cancelled)),
            },
            "{mode}: {result:?}"
        );
        runtime.shutdown().await;
    }
}

#[tokio::test]
async fn snapshot_read_indexes_admit_descriptor_pages_and_warm_semantic_results() {
    for mode in ["descriptor", "page", "warm"] {
        let fixture = open_fixture(1, None).await;
        let snapshot = snapshot_with_read_indexes(&fixture, true).await;
        let runtime = Arc::new(RemoteGitRuntime::default());
        let cancel = CancellationToken::new();
        let repository = RemoteGitRepository::from_snapshot(
            fixture.layout.clone(),
            &snapshot,
            RepositoryIdentity::new("memory", "org/repo", 1).unwrap(),
            runtime.clone(),
            RepositoryOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        let paths = [crate::GitPath::new(b"README.md".to_vec()).unwrap()];
        if mode != "descriptor" {
            let warm = repository
                .operation(crate::OperationKind::History, &cancel)
                .await
                .unwrap();
            let index = repository.state.path_state(&warm).await.unwrap().unwrap();
            if mode == "warm" {
                index
                    .latest(
                        &repository.state.store,
                        ObjectId::Sha1([0x11; 20]),
                        &paths,
                        &warm,
                    )
                    .await
                    .unwrap();
            }
            warm.finish(Ok(())).await.unwrap();
        }
        let limits = match mode {
            "descriptor" => OperationLimits {
                max_fetched_bytes: 1,
                ..Default::default()
            },
            "page" => OperationLimits {
                max_storage_requests: 1,
                ..Default::default()
            },
            _ => OperationLimits {
                max_response_bytes: 1,
                ..Default::default()
            },
        };
        let operation = crate::OperationContext::open(
            repository.state.clone(),
            crate::OperationKind::History,
            &cancel,
            limits,
        )
        .await
        .unwrap();
        let result = async {
            let index = repository.state.path_state(&operation).await?.unwrap();
            index
                .latest(
                    &repository.state.store,
                    ObjectId::Sha1([0x11; 20]),
                    &paths,
                    &operation,
                )
                .await
        }
        .await;
        let error = operation.finish(result).await.unwrap_err();
        assert!(
            match mode {
                "warm" => matches!(
                    error,
                    Error::LimitExceeded {
                        limit: "response bytes",
                        ..
                    }
                ),
                _ => admission_rejected(&error),
            },
            "{mode}: {error:?}"
        );
        runtime.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_read_indexes_cancel_inflight_dependency_pages() {
    for (mode, dependency) in ["caller", "shutdown", "deadline"]
        .into_iter()
        .flat_map(|mode| ["path-state", "commit-graph"].map(|dependency| (mode, dependency)))
    {
        let fixture = open_fixture(1, None).await;
        let snapshot = snapshot_with_read_indexes(&fixture, true).await;
        let runtime = Arc::new(RemoteGitRuntime::default());
        let cancel = CancellationToken::new();
        let repository = RemoteGitRepository::from_snapshot(
            fixture.layout.clone(),
            &snapshot,
            RepositoryIdentity::new("memory", "org/repo", 1).unwrap(),
            runtime.clone(),
            RepositoryOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
        fixture
            .backend
            .block_path_containing(if dependency == "path-state" {
                "metadata/path-state/layers/"
            } else {
                "metadata/commit-graph/layers/"
            });
        let worker_cancel = cancel.clone();
        let worker = tokio::spawn(async move {
            let limits = OperationLimits {
                max_duration: if mode == "deadline" {
                    Duration::from_millis(50)
                } else {
                    Duration::from_secs(60)
                },
                ..Default::default()
            };
            let operation = crate::OperationContext::open(
                repository.state.clone(),
                crate::OperationKind::History,
                &worker_cancel,
                limits,
            )
            .await
            .unwrap();
            let result = async {
                if dependency == "commit-graph" {
                    return repository
                        .state
                        .commit_graph
                        .as_ref()
                        .unwrap()
                        .is_reachable(ObjectId::Sha1([0x11; 20]), &operation)
                        .await
                        .map(|_| ());
                }
                let index = repository.state.path_state(&operation).await?.unwrap();
                index
                    .latest(
                        &repository.state.store,
                        ObjectId::Sha1([0x11; 20]),
                        &[crate::GitPath::new(b"README.md".to_vec()).unwrap()],
                        &operation,
                    )
                    .await
                    .map(|_| ())
            }
            .await;
            operation.finish(result).await
        });
        tokio::time::timeout(
            Duration::from_secs(2),
            fixture.backend.request_started.notified(),
        )
        .await
        .unwrap();
        if mode == "caller" {
            cancel.cancel();
        }
        if mode == "shutdown" {
            tokio::time::timeout(Duration::from_secs(2), runtime.shutdown())
                .await
                .unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(
            match mode {
                "deadline" => matches!(result, Err(Error::Timeout { .. })),
                _ => matches!(result, Err(Error::Cancelled)),
            },
            "{mode}/{dependency}: {result:?}"
        );
        runtime.shutdown().await;
    }
}

#[tokio::test]
async fn released_manifest_graph_remains_usable_for_metadata_membership() {
    let fixture = open_fixture(1, None).await;
    let snapshot = snapshot_with_read_indexes(&fixture, false).await;
    let runtime = Arc::new(RemoteGitRuntime::default());
    let cancel = CancellationToken::new();
    let repository = RemoteGitRepository::from_snapshot(
        fixture.layout.clone(),
        &snapshot,
        RepositoryIdentity::new("memory", "org/repo", 1).unwrap(),
        runtime.clone(),
        RepositoryOptions::default(),
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(
        repository
            .commits_reachable_from(
                &[
                    ObjectId::Sha1([0x11; 20]),
                    ObjectId::Sha1([0x99; 20]),
                    ObjectId::Sha1([0x11; 20])
                ],
                &[ObjectId::Sha1([0x11; 20])],
                &cancel,
            )
            .await
            .unwrap(),
        Some(vec![true, false, true])
    );
    runtime.shutdown().await;
}

#[tokio::test]
async fn paged_graph_queries_preserve_merge_parents_and_exclude_retained_history() {
    use crab_metadata::split_commit_graph::{
        CommitGraphInput, append_paged_commit_graph, upload_split_commit_graph,
    };
    let fixture = open_fixture(1, None).await;
    let mut snapshot = snapshot_with_read_indexes(&fixture, true).await;
    let input = |value: u8, parents: &[u8]| CommitGraphInput {
        oid: [value; 20],
        tree_oid: [0x22; 20],
        commit_time: i64::from(value),
        parents: parents.iter().map(|parent| [*parent; 20]).collect(),
    };
    let graph = append_paged_commit_graph(
        None,
        snapshot.manifest.generation,
        snapshot.manifest.pack_index_hash.clone(),
        snapshot.manifest.git_validation_digest.clone(),
        &[[0x11; 20], [4; 20]],
        vec![
            input(1, &[]),
            input(2, &[1]),
            input(3, &[1]),
            input(0x11, &[2, 3]),
            input(4, &[1]),
        ],
    )
    .unwrap()
    .unwrap();
    upload_split_commit_graph(&fixture.store, &fixture.layout, &graph)
        .await
        .unwrap();
    snapshot.manifest.commit_graph_hash = Some(graph.descriptor_hash);
    snapshot.manifest.path_state_hash = None;
    let runtime = Arc::new(RemoteGitRuntime::default());
    let cancel = CancellationToken::new();
    let repository = RemoteGitRepository::from_snapshot(
        fixture.layout.clone(),
        &snapshot,
        RepositoryIdentity::new("memory", "org/repo", 1).unwrap(),
        runtime.clone(),
        RepositoryOptions::default(),
        &cancel,
    )
    .await
    .unwrap();
    let operation = repository
        .operation(crate::OperationKind::Resolve, &cancel)
        .await
        .unwrap();
    let result = async {
        let graph = repository.state.commit_graph.as_ref().unwrap();
        assert!(matches!(
            graph.view(),
            crab_metadata::split_commit_graph::CommitGraphView::Paged(_)
        ));
        let candidates = [1, 2, 3, 4, 0x11, 99, 2].map(|value| ObjectId::Sha1([value; 20]));
        assert_eq!(
            graph
                .reachable_from_roots(&candidates, &[ObjectId::Sha1([0x11; 20])], &operation)
                .await?,
            vec![true, true, true, false, true, false, true]
        );
        assert!(
            !graph
                .is_reachable(ObjectId::Sha1([4; 20]), &operation)
                .await?
        );
        assert!(matches!(
            repository
                .resolve(&Revision::Commit(ObjectId::Sha1([4; 20])), &operation)
                .await,
            Err(Error::Revision {
                reason: RevisionError::NotReachable
            })
        ));
        assert_eq!(
            graph
                .reachable_from_roots(&candidates, &[ObjectId::Sha1([1; 20])], &operation)
                .await?,
            vec![true, false, false, false, false, false, false]
        );
        assert_eq!(
            graph
                .first_parent_chain(ObjectId::Sha1([0x11; 20]), 2, &operation)
                .await?,
            Some(vec![ObjectId::Sha1([0x11; 20]), ObjectId::Sha1([2; 20])])
        );
        assert!(
            graph
                .first_parent_chain(ObjectId::Sha1([99; 20]), 2, &operation)
                .await?
                .is_none()
        );
        Ok(())
    }
    .await;
    operation.finish(result).await.unwrap();
    runtime.shutdown().await;
}
