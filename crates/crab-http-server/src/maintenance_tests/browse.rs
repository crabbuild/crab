use super::*;
use crab_metadata::capsule_protocol::{BrowseIndexes, load_browse_indexes};
use crab_remote_git::OperationKind;

#[tokio::test]
async fn projection_reads_reuse_the_runtime_cell_during_background_ingestion() {
    let server = fixture().await;
    let repo = repository(&server);
    let router = server.repository_cells.as_ref().unwrap();
    crate::projection::state(router, repo.id).await.unwrap();
    // A read-only cold activation is owned but not yet Residency::Resident.
    // Count retained actors rather than using that narrower routing probe.
    let importer = router.repository(repo.id).unwrap();
    let state = tokio::time::timeout(
        Duration::from_secs(1),
        crate::projection::state(router, repo.id),
    )
    .await;
    drop(importer);
    close(&server).await;
    assert!(matches!(state, Ok(Ok(_))));
}

#[tokio::test]
async fn projected_attribution_survives_force_push_rewind_with_retained_graph_records() {
    let server = fixture().await;
    let repo = repository(&server);
    let (history, _) = crate::test_git::merge_history_with_nested_tag();
    let mut root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    let mut old = None;
    let mut responses = Vec::new();
    for index in [3, 0] {
        let published = publish_next(
            &repo,
            root,
            old,
            history.oids[index].clone(),
            (index == 3).then(|| history.pack.clone()),
        )
        .await;
        root = published.0;
        old = Some(published.1);
        let result = crate::maintenance::run(
            repo.layout.clone(),
            Arc::clone(&server.maintenance_admission),
            None,
            server.cancellation.clone(),
            crate::maintenance::Pass::BackgroundMaintenance,
            Some(crate::maintenance::ProjectionContext {
                repository_id: repo.id,
                client: server
                    .repository_cells
                    .as_ref()
                    .map(|router| (**router).clone()),
                metrics: server.metrics.clone(),
                identity: repo.identity.clone(),
                runtime: Arc::clone(&server.runtime),
                options: server.options,
            }),
        )
        .await;
        if !matches!(result, Ok(Some(_))) {
            close(&server).await;
            panic!("projection did not promote after publishing commit {index}: {result:?}");
        }
        repo.invalidate().await;
        let response = router(Arc::clone(&server))
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/repos/team/repo/tree-attribution?rev={}&limit=100",
                        history.oids[index]
                    ))
                    .header("host", "localhost:8788")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        responses.push((index, status, value));
    }
    let indexes = load_browse_indexes(&repo.layout).await.unwrap().unwrap();
    let graph = crab_metadata::split_commit_graph::load_split_commit_graph(
        &repo.store,
        &repo.layout,
        indexes.commit_graph_hash(),
        server.options.object_limits().max_commit_graph_bytes,
    )
    .await
    .unwrap();
    let merge = gix_hash::ObjectId::from_hex(history.oids[3].as_bytes()).unwrap();
    let retained = graph
        .ordinal(merge.as_bytes().try_into().unwrap())
        .is_some();
    // Projection summaries include retained ordinals. The foreground revision
    // boundary must still reject history removed by the current ref rewind.
    let hidden = router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/repos/team/repo/tree-attribution?rev={}&limit=100",
                    history.oids[3]
                ))
                .header("host", "localhost:8788")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    close(&server).await;
    assert_eq!((retained, hidden), (true, StatusCode::NOT_FOUND));
    assert!(
        responses
            .iter()
            .all(|(index, status, value)| *status == StatusCode::OK
                && value["items"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty()
                        && (*index != 0
                            || items
                                .iter()
                                .all(|entry| entry["last_commit"]["oid"] == history.oids[0])))),
        "{responses:?}"
    );
}

#[tokio::test]
async fn capsule_merge_and_nested_tag_preserve_graph_and_first_parent_attribution() {
    let mut server = fixture_without_cells().await;
    let counting = {
        let mut current = server.repositories.current.write().unwrap();
        let repo = Arc::get_mut(current.by_name.values_mut().next().unwrap()).unwrap();
        let counting = Arc::new(BrowseFaultStore::new(Arc::clone(repo.store.inner())));
        repo.store = Store::new(counting.clone());
        repo.layout = StoreLayout::new(repo.store.clone(), "maintenance".into());
        counting
    };
    Arc::get_mut(&mut server).unwrap().options = RepositoryOptions::new(
        server.options.object_limits(),
        crab_remote_git::OperationLimits {
            max_history_commits: 1,
            ..server.options.operation_limits()
        },
    )
    .unwrap();
    let repo = repository(&server);
    let (history, tag) = crate::test_git::merge_history_with_nested_tag();
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![
            CapsuleRefEdit::new("refs/heads/main", None, Some(history.oids[3].clone()), None),
            CapsuleRefEdit::new(
                "refs/tags/outer",
                None,
                Some(tag),
                Some(history.oids[2].clone()),
            ),
        ],
    )
    .unwrap();
    let capsule = Capsule::build(&transaction, vec![history.pack], Vec::new()).unwrap();
    crab_write::capsule_protocol::publish(&repo.layout, root, &transaction, &capsule)
        .await
        .unwrap();
    counting.reset();
    crab_remote::browse_indexes::ensure(
        &repo.layout,
        &repo.identity,
        Arc::clone(&server.runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let construction_reads = counting.body_requests();
    let indexes = load_browse_indexes(&repo.layout).await.unwrap().unwrap();
    let graph = crab_metadata::split_commit_graph::load_split_commit_graph(
        &repo.store,
        &repo.layout,
        indexes.commit_graph_hash(),
        server.options.object_limits().max_commit_graph_bytes,
    )
    .await
    .unwrap();
    let paths = crab_metadata::path_state::load_path_state(
        &repo.store,
        &repo.layout,
        indexes.path_state_hash().unwrap(),
        &graph,
        server.options.object_limits().max_path_state_bytes,
    )
    .await
    .unwrap();
    let merge: [u8; 20] = gix_hash::ObjectId::from_hex(history.oids[3].as_bytes())
        .unwrap()
        .as_bytes()
        .try_into()
        .unwrap();
    let record = graph.record(graph.ordinal(&merge).unwrap()).unwrap();
    let parents = record
        .parents
        .iter()
        .map(|ordinal| gix_hash::ObjectId::from(graph.record(*ordinal).unwrap().oid).to_string())
        .collect::<Vec<_>>();
    let attribution = paths.latest(&graph, &merge, &[b"asset"]).unwrap();
    close(&server).await;
    assert_eq!(
        (
            parents,
            gix_hash::ObjectId::from(attribution[0].oid).to_string()
        ),
        (history.oids[1..3].to_vec(), history.oids[0].clone())
    );
    eprintln!("capsule merge/tag construction: {construction_reads} body reads");
}

#[tokio::test]
async fn graph_publication_survives_failed_attribution_and_reader_restart() {
    let server = fixture_without_cells().await;
    let counting = {
        let mut current = server.repositories.current.write().unwrap();
        let repo = Arc::get_mut(current.by_name.values_mut().next().unwrap()).unwrap();
        let counting = Arc::new(BrowseFaultStore::new(Arc::clone(repo.store.inner())));
        repo.store = Store::with_retry(
            counting.clone(),
            crab_storage::RetryPolicy {
                max_attempts: 1,
                ..Default::default()
            },
        );
        repo.layout = StoreLayout::new(repo.store.clone(), "maintenance".into());
        counting
    };
    let repo = repository(&server);
    let history = git_history(64);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    publish_next(
        &repo,
        root,
        None,
        history.oids[63].clone(),
        Some(history.pack),
    )
    .await;
    let snapshot = repo.open_view().await.unwrap().git_snapshot().unwrap();
    let checkpoint_path =
        repo.layout
            .repo_path(&crab_metadata::path_state::path_state_checkpoint_path(
                &snapshot.manifest.git_validation_digest,
            ));
    counting.block_body_reads_for(&checkpoint_path);
    let first_runtime = Arc::new(RemoteGitRuntime::default());
    let failed = crab_remote::browse_indexes::ensure(
        &repo.layout,
        &repo.identity,
        Arc::clone(&first_runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &CancellationToken::new(),
    )
    .await;
    first_runtime.shutdown().await;
    assert!(failed.is_err());
    let graph_only = load_browse_indexes(&repo.layout).await.unwrap().unwrap();
    assert!(graph_only.path_state_hash().is_none());

    // Keep the blocked builder from racing these reads. Partial readiness must
    // serve historical commits and trees while attribution reports indexing.
    let maintenance = server
        .maintenance_admission
        .clone()
        .acquire_many_owned(2)
        .await
        .unwrap();
    for (action, expected) in [
        ("commit", StatusCode::OK),
        ("tree", StatusCode::OK),
        ("tree-attribution", StatusCode::ACCEPTED),
    ] {
        let uri = format!("/api/repos/team/repo/{action}?rev={}", history.oids[0]);
        let response = router(Arc::clone(&server))
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("host", "localhost:8788")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{action}");
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        if action == "commit" {
            let commit: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(commit["oid"], history.oids[0]);
        }
    }
    drop(maintenance);

    // A fresh runtime opens the graph published by the failed builder. The
    // same-state complete publication must then replace its cached partial view.
    let before = repo.open(&server, &CancellationToken::new()).await.unwrap();
    let operation = before
        .git
        .operation(OperationKind::Commit, &CancellationToken::new())
        .await
        .unwrap();
    let historical = before
        .git
        .snapshot(
            &crab_remote_git::Revision::Commit(
                gix_hash::ObjectId::from_hex(history.oids[0].as_bytes()).unwrap(),
            ),
            &operation,
        )
        .await;
    let historical = operation.finish(historical).await.unwrap();
    assert_eq!(historical.commit_oid().to_string(), history.oids[0]);
    counting.unblock_body_reads_for(&checkpoint_path);
    crab_remote::browse_indexes::ensure(
        &repo.layout,
        &repo.identity,
        Arc::clone(&server.runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    repo.pinned.lock().await.as_mut().unwrap().checked =
        Some(Instant::now() - Duration::from_secs(3));
    let after = repo.open(&server, &CancellationToken::new()).await.unwrap();
    close(&server).await;
    assert_eq!(
        (
            before.git.commit_graph_available(),
            before.git.path_state_available(),
            after.git.path_state_available()
        ),
        (true, false, true)
    );
}

#[derive(Default)]
struct CompletedOperations(AtomicUsize);

impl crab_remote_git::RemoteGitMetrics for CompletedOperations {
    fn record(&self, observation: crab_remote_git::MetricObservation) {
        if observation.kind == crab_remote_git::MetricKind::Operation {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[tokio::test]
async fn reusable_complete_prefix_repairs_a_corrupt_successor_checkpoint() {
    let server = fixture_without_cells().await;
    let repo = repository(&server);
    let history = git_history(64);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    let (root, old) = publish_next(
        &repo,
        root,
        None,
        history.oids[63].clone(),
        Some(history.pack),
    )
    .await;
    crab_remote::browse_indexes::ensure(
        &repo.layout,
        &repo.identity,
        Arc::clone(&server.runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let previous = load_browse_indexes(&repo.layout).await.unwrap().unwrap();
    let graph = crab_metadata::split_commit_graph::load_split_commit_graph(
        &repo.store,
        &repo.layout,
        previous.commit_graph_hash(),
        server.options.object_limits().max_commit_graph_bytes,
    )
    .await
    .unwrap();
    publish_next(&repo, root, Some(old), history.oids[32].clone(), None).await;
    let current = repo.open_view().await.unwrap().git_snapshot().unwrap();
    let checkpoint = crab_metadata::path_state::PathStateCheckpoint {
        version: 1,
        generation: current.manifest.generation,
        pack_index_hash: current.manifest.pack_index_hash.clone(),
        git_validation_digest: current.manifest.git_validation_digest.clone(),
        commit_ordinal_digest: graph.ordinal_digest(),
        commit_count: 64,
        descriptor_hash: "f".repeat(64),
    };
    let checkpoint_path =
        repo.layout
            .repo_path(&crab_metadata::path_state::path_state_checkpoint_path(
                &current.manifest.git_validation_digest,
            ));
    repo.store
        .put_overwrite(
            &checkpoint_path,
            Bytes::from(serde_json::to_vec(&checkpoint).unwrap()),
        )
        .await
        .unwrap();
    let repaired = crab_remote::browse_indexes::ensure(
        &repo.layout,
        &repo.identity,
        Arc::clone(&server.runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &CancellationToken::new(),
    )
    .await;
    close(&server).await;
    assert!(
        repaired.is_ok(),
        "verified reusable prefix was lost: {repaired:?}"
    );
}

#[tokio::test]
async fn retired_attribution_semantics_rebuild_completion_and_checkpoint() {
    for complete_record in [false, true] {
        let server = fixture_without_cells().await;
        let repo = repository(&server);
        let history = git_history(64);
        let root = crab_write::capsule_protocol::open_root(&repo.layout)
            .await
            .unwrap();
        publish_next(
            &repo,
            root,
            None,
            history.oids[63].clone(),
            Some(history.pack),
        )
        .await;
        let build = || {
            crab_remote::browse_indexes::ensure(
                &repo.layout,
                &repo.identity,
                Arc::clone(&server.runtime),
                server.options,
                2 * 1024 * 1024 * 1024,
                &server.cancellation,
            )
        };
        let expected = build().await.unwrap().unwrap();
        let graph = crab_metadata::split_commit_graph::load_split_commit_graph(
            &repo.store,
            &repo.layout,
            expected.commit_graph_hash(),
            server.options.object_limits().max_commit_graph_bytes,
        )
        .await
        .unwrap();
        let mut descriptor = crab_metadata::path_state::load_path_state_descriptor(
            &repo.store,
            &repo.layout,
            expected.path_state_hash().unwrap(),
            server.options.object_limits().max_path_state_bytes,
        )
        .await
        .unwrap();
        // A valid content hash cannot prove mutations made by retired comparison
        // semantics. Completion and resumable progress must both be rebuilt.
        descriptor.version = 3;
        let bytes = serde_json::to_vec(&descriptor).unwrap();
        let retired_hash = blake3::hash(&bytes).to_hex().to_string();
        repo.store
            .put_overwrite(
                &repo.layout.bulk_manifest_path("path-state", &retired_hash),
                Bytes::from(bytes),
            )
            .await
            .unwrap();
        let retired_rejected = crab_metadata::path_state::load_path_state_descriptor(
            &repo.store,
            &repo.layout,
            &retired_hash,
            server.options.object_limits().max_path_state_bytes,
        )
        .await
        .is_err();
        let checkpoint = crab_metadata::path_state::PathStateCheckpoint {
            version: 1,
            generation: graph.descriptor.generation,
            pack_index_hash: graph.descriptor.pack_index_hash.clone(),
            git_validation_digest: graph.descriptor.git_validation_digest.clone(),
            commit_ordinal_digest: graph.ordinal_digest(),
            commit_count: 64,
            descriptor_hash: retired_hash.clone(),
        };
        let checkpoint_path =
            repo.layout
                .repo_path(&crab_metadata::path_state::path_state_checkpoint_path(
                    &graph.descriptor.git_validation_digest,
                ));
        repo.store
            .put_overwrite(
                &checkpoint_path,
                Bytes::from(serde_json::to_vec(&checkpoint).unwrap()),
            )
            .await
            .unwrap();
        let retired = BrowseIndexes::new(
            expected.state_digest().to_owned(),
            expected.commit_graph_hash().to_owned(),
            complete_record.then_some(retired_hash),
        )
        .unwrap();
        repo.store
            .put_overwrite(
                &repo.layout.capsule_browse_indexes_path(),
                retired.encode().unwrap(),
            )
            .await
            .unwrap();
        let rebuilt = build().await;
        let checkpoint = crab_metadata::path_state::load_path_state_checkpoint_record(
            &repo.store,
            &repo.layout,
            &graph.descriptor.git_validation_digest,
        )
        .await;
        close(&server).await;
        assert!(
            retired_rejected,
            "validly hashed retired semantics were accepted"
        );
        assert_eq!(rebuilt.unwrap(), Some(expected));
        assert_eq!(checkpoint.unwrap().unwrap().commit_count, 32);
    }
}

#[tokio::test]
async fn attribution_restart_reuses_the_verified_thirty_two_commit_checkpoint() {
    let server = fixture_without_cells().await;
    let repo = repository(&server);
    let history = git_history(64);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    publish_next(
        &repo,
        root,
        None,
        history.oids[63].clone(),
        Some(history.pack),
    )
    .await;
    crab_remote::browse_indexes::ensure(
        &repo.layout,
        &repo.identity,
        Arc::clone(&server.runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let complete = load_browse_indexes(&repo.layout).await.unwrap().unwrap();
    let partial = BrowseIndexes::new(
        complete.state_digest().to_owned(),
        complete.commit_graph_hash().to_owned(),
        None,
    )
    .unwrap();
    let view = repo
        .open_view()
        .await
        .unwrap()
        .with_browse_indexes(Some(partial.clone()));
    let snapshot = view.git_snapshot().unwrap();
    let progress = crab_metadata::path_state::load_path_state_checkpoint_record(
        &repo.store,
        &repo.layout,
        &snapshot.manifest.git_validation_digest,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(progress.commit_count, 32);
    let metrics = Arc::new(CompletedOperations::default());
    let runtime = Arc::new(
        RemoteGitRuntime::new(crab_remote_git::RuntimeOptions::default(), metrics.clone()).unwrap(),
    );
    let reader = view
        .git_repository_from_store(
            repo.layout.clone(),
            repo.identity.clone(),
            Arc::clone(&runtime),
            server.options,
            2 * 1024 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    metrics.0.store(0, Ordering::Relaxed);
    let resumed = crab_write::generation::browse::build_path_state(
        &repo.layout,
        &reader,
        &snapshot,
        &partial,
        server.options,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    runtime.shutdown().await;
    close(&server).await;
    // One graph attachment, then comparisons for the remaining 32 commits.
    // Byte-identical completion and the operation count prove prefix reuse.
    assert_eq!((resumed, metrics.0.load(Ordering::Relaxed)), (complete, 33));
}

#[tokio::test]
async fn superseded_partial_graph_does_not_authorize_retained_history() {
    let server = fixture_without_cells().await;
    let repo = repository(&server);
    let history = git_history(64);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    let (root, old) = publish_next(
        &repo,
        root,
        None,
        history.oids[63].clone(),
        Some(history.pack),
    )
    .await;
    let captured = repo.open_view().await.unwrap();
    let snapshot = captured.git_snapshot().unwrap();
    let reader = captured
        .git_repository_from_store(
            repo.layout.clone(),
            repo.identity.clone(),
            Arc::clone(&server.runtime),
            server.options,
            2 * 1024 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let partial = crab_write::generation::browse::build_graph(
        &repo.layout,
        &reader,
        &snapshot,
        None,
        server.options,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    repo.store
        .create_strict(
            &repo.layout.capsule_browse_indexes_path(),
            partial.encode().unwrap(),
        )
        .await
        .unwrap();
    publish_next(&repo, root, Some(old), history.oids[32].clone(), None).await;
    crab_remote::browse_indexes::ensure(
        &repo.layout,
        &repo.identity,
        Arc::clone(&server.runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let current = load_browse_indexes(&repo.layout).await.unwrap().unwrap();
    let graph = crab_metadata::split_commit_graph::load_split_commit_graph(
        &repo.store,
        &repo.layout,
        current.commit_graph_hash(),
        server.options.object_limits().max_commit_graph_bytes,
    )
    .await
    .unwrap();
    close(&server).await;
    assert_eq!(graph.descriptor.commit_count, 33);
}

#[tokio::test]
async fn unchanged_projection_probe_reuses_its_verified_descriptor() {
    for commit_count in [4, 64, 256] {
        let server = fixture_without_cells().await;
        let counting = {
            let mut current = server.repositories.current.write().unwrap();
            let repo = Arc::get_mut(current.by_name.values_mut().next().unwrap()).unwrap();
            let counting = Arc::new(BrowseFaultStore::new(Arc::clone(repo.store.inner())));
            repo.store = Store::with_retry(
                counting.clone(),
                crab_storage::RetryPolicy {
                    max_attempts: 1,
                    ..Default::default()
                },
            );
            repo.layout = StoreLayout::new(repo.store.clone(), "maintenance".into());
            counting
        };
        let repo = repository(&server);
        let history = git_history(commit_count);
        let root = crab_write::capsule_protocol::open_root(&repo.layout)
            .await
            .unwrap();
        publish_next(
            &repo,
            root,
            None,
            history.oids[commit_count - 1].clone(),
            Some(history.pack),
        )
        .await;
        crab_remote::browse_indexes::ensure(
            &repo.layout,
            &repo.identity,
            Arc::clone(&server.runtime),
            server.options,
            2 * 1024 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let source = crate::projection::current_source(&repo, &server, &server.cancellation)
            .await
            .unwrap();
        let descriptor = repo
            .layout
            .bulk_manifest_path("path-state", source.path_state_hash.as_deref().unwrap());
        counting.block_body_reads_for(&descriptor);
        counting.reset();
        tokio::time::sleep(Duration::from_millis(2100)).await;
        repo.open(&server, &server.cancellation).await.unwrap();
        let repeated =
            crate::projection::current_source(&repo, &server, &server.cancellation).await;
        let body_requests = counting.body_requests();
        let list_requests = counting.list_requests();
        close(&server).await;
        assert!(
            repeated.is_ok(),
            "unchanged source reopened its blocked immutable descriptor ({body_requests} body reads)"
        );
        assert_eq!(
            (body_requests, list_requests),
            (3, 2),
            "unchanged probe at {commit_count} commits exceeded its fixed-ref control reads"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn source_probe_cancels_a_pending_cold_control_read() {
    let server = fixture_without_cells().await;
    let counting = {
        let mut current = server.repositories.current.write().unwrap();
        let repo = Arc::get_mut(current.by_name.values_mut().next().unwrap()).unwrap();
        let counting = Arc::new(BrowseFaultStore::new(Arc::clone(repo.store.inner())));
        repo.store = Store::new(counting.clone());
        repo.layout = StoreLayout::new(repo.store.clone(), "maintenance".into());
        counting
    };
    let repo = repository(&server);
    let started = counting.park_body_reads_for(&repo.layout.capsule_root_path());
    let cancellation = CancellationToken::new();
    let mut worker = tokio::spawn({
        let server = Arc::clone(&server);
        let cancellation = cancellation.clone();
        async move { crate::projection::current_source(&repo, &server, &cancellation).await }
    });
    tokio::time::timeout(Duration::from_secs(30), started.notified())
        .await
        .unwrap();
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), &mut worker).await;
    if result.is_err() {
        worker.abort();
        let _ = worker.await;
    }
    close(&server).await;
    assert!(matches!(
        result,
        Ok(Ok(Err(crate::Error::Remote(
            crab_remote_git::Error::Cancelled
        ))))
    ));
}

#[derive(Debug)]
struct BrowseFaultStore {
    inner: Arc<dyn object_store::ObjectStore>,
    body_reads: AtomicUsize,
    lists: AtomicUsize,
    blocked: std::sync::Mutex<std::collections::BTreeSet<String>>,
    parked: std::sync::Mutex<Option<(String, Arc<tokio::sync::Notify>)>>,
}

impl BrowseFaultStore {
    fn new(inner: Arc<dyn object_store::ObjectStore>) -> Self {
        Self {
            inner,
            body_reads: AtomicUsize::new(0),
            lists: AtomicUsize::new(0),
            blocked: Default::default(),
            parked: Default::default(),
        }
    }

    fn block_body_reads_for(&self, path: &ObjectPath) {
        self.blocked.lock().unwrap().insert(path.to_string());
    }

    fn unblock_body_reads_for(&self, path: &ObjectPath) {
        self.blocked.lock().unwrap().remove(path.as_ref());
    }

    fn park_body_reads_for(&self, path: &ObjectPath) -> Arc<tokio::sync::Notify> {
        let started = Arc::new(tokio::sync::Notify::new());
        *self.parked.lock().unwrap() = Some((path.to_string(), Arc::clone(&started)));
        started
    }

    fn reset(&self) {
        self.body_reads.store(0, Ordering::Relaxed);
        self.lists.store(0, Ordering::Relaxed);
    }

    fn body_requests(&self) -> usize {
        self.body_reads.load(Ordering::Relaxed)
    }

    fn list_requests(&self) -> usize {
        self.lists.load(Ordering::Relaxed)
    }
}

impl std::fmt::Display for BrowseFaultStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("browse-fault-store")
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for BrowseFaultStore {
    async fn put_opts(
        &self,
        path: &ObjectPath,
        payload: object_store::PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &ObjectPath,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    async fn get_opts(
        &self,
        path: &ObjectPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if !options.head {
            self.body_reads.fetch_add(1, Ordering::Relaxed);
            // Release the synchronous guard before awaiting the real provider.
            let blocked = self.blocked.lock().unwrap().contains(path.as_ref());
            if blocked {
                return Err(object_store::Error::Generic {
                    store: "browse-fault-store",
                    source: Box::new(std::io::Error::other("immutable body read blocked")),
                });
            }
            let parked = self
                .parked
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(location, _)| location == path.as_ref())
                .map(|(_, started)| Arc::clone(started));
            if let Some(started) = parked {
                started.notify_one();
                std::future::pending::<()>().await;
            }
        }
        self.inner.get_opts(path, options).await
    }

    fn delete_stream(
        &self,
        paths: futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(paths)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.lists.fetch_add(1, Ordering::Relaxed);
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<object_store::ListResult> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}
