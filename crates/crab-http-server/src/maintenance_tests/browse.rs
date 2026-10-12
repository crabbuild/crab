use super::*;
use crab_metadata::capsule_protocol::{BrowseIndexes, load_browse_indexes};
use crab_remote_git::OperationKind;

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
        .operation(OperationKind::Commit, &CancellationToken::new())
        .await
        .unwrap();
    let historical = before
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
    repo.pinned.lock().await.as_mut().unwrap().checked = Instant::now() - Duration::from_secs(3);
    let after = repo.open(&server, &CancellationToken::new()).await.unwrap();
    close(&server).await;
    assert_eq!(
        (
            before.commit_graph_available(),
            before.path_state_available(),
            after.path_state_available()
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

#[derive(Debug)]
struct BrowseFaultStore {
    inner: Arc<dyn object_store::ObjectStore>,
    body_reads: AtomicUsize,
    blocked: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

impl BrowseFaultStore {
    fn new(inner: Arc<dyn object_store::ObjectStore>) -> Self {
        Self {
            inner,
            body_reads: AtomicUsize::new(0),
            blocked: Default::default(),
        }
    }

    fn block_body_reads_for(&self, path: &ObjectPath) {
        self.blocked.lock().unwrap().insert(path.to_string());
    }

    fn unblock_body_reads_for(&self, path: &ObjectPath) {
        self.blocked.lock().unwrap().remove(path.as_ref());
    }

    fn reset(&self) {
        self.body_reads.store(0, Ordering::Relaxed);
    }

    fn body_requests(&self) -> usize {
        self.body_reads.load(Ordering::Relaxed)
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
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<object_store::ListResult> {
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
