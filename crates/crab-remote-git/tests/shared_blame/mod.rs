use super::*;

#[derive(Default)]
struct Producers(AtomicUsize);

impl crab_remote_git::RemoteGitMetrics for Producers {
    fn record(&self, event: crab_remote_git::MetricObservation) {
        if event.kind == crab_remote_git::MetricKind::BlameProducer && event.duration.is_none() {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

async fn wait_for_waiters(runtime: &RemoteGitRuntime, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if runtime.snapshot().await.active_blame_waiters == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("blame waiters register");
}

async fn prepare(
    fixture: &PublishedFixture,
    warm_blob: bool,
) -> crab_remote_git::RemoteGitSnapshot {
    prepare_at(fixture, &fixture.target_path, warm_blob).await
}

async fn prepare_at(
    fixture: &PublishedFixture,
    path: &GitPath,
    warm_blob: bool,
) -> crab_remote_git::RemoteGitSnapshot {
    let cancel = CancellationToken::new();
    let operation = fixture
        .repository
        .operation(OperationKind::Blame, &cancel)
        .await
        .unwrap();
    let snapshot = fixture
        .repository
        .snapshot(&Revision::Reference("main".into()), &operation)
        .await
        .unwrap();
    snapshot.entry(path, &operation).await.unwrap();
    if warm_blob {
        snapshot.read_blob(path, &operation).await.unwrap();
    }
    operation.finish(Ok(())).await.unwrap();
    snapshot
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_blame_has_one_producer_and_independent_waiter_limits() {
    for strict_first in [true, false] {
        let metrics = Arc::new(Producers::default());
        let fixture = publish_with_metrics(
            DeltaKind::Ref,
            false,
            RepositoryOptions::default(),
            RuntimeOptions::default(),
            metrics.clone(),
        )
        .await;
        let snapshot = prepare(&fixture, false).await;
        fixture.backend.block_next_pack_get();
        let mut tasks = Vec::new();
        for index in 0..16 {
            let strict = if strict_first {
                index == 0
            } else {
                index == 15
            };
            let limits = OperationLimits {
                max_blame_lines: if strict {
                    1
                } else {
                    OperationLimits::default().max_blame_lines
                },
                max_history_commits: 20_000,
                ..OperationLimits::default()
            };
            let cancel = CancellationToken::new();
            let operation = fixture
                .repository
                .operation_with_limits(OperationKind::Blame, &cancel, limits)
                .await
                .unwrap();
            let snapshot = snapshot.clone();
            let path = fixture.target_path.clone();
            tasks.push((
                strict,
                tokio::spawn(async move {
                    let result = snapshot.shared_blame(&path, &operation).await;
                    operation.finish(result).await
                }),
            ));
            if index == 0 {
                fixture.backend.wait_for_blocked_pack_get().await;
            }
        }
        wait_for_waiters(&fixture.runtime, 16).await;
        fixture.backend.release_blocked_pack_get();
        for (strict, task) in tasks {
            let result = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            if strict {
                assert!(matches!(
                    result,
                    Err(Error::LimitExceeded {
                        limit: "blame lines",
                        maximum: 1,
                        ..
                    })
                ));
            } else {
                let blame = result.unwrap();
                assert_eq!(blame.ranges[0].commit.oid, fixture.root_commit);
            }
        }
        assert_eq!(metrics.0.load(Ordering::SeqCst), 1);
        fixture.runtime.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn late_blame_waiters_replay_semantic_work_and_maximum_depth() {
    for depth in [false, true] {
        let metrics = Arc::new(Producers::default());
        let fixture = publish_with_metrics(
            DeltaKind::Ref,
            false,
            RepositoryOptions::default(),
            RuntimeOptions::default(),
            metrics.clone(),
        )
        .await;
        let path = if depth {
            GitPath::new(b"dir/nested/deep.txt".to_vec()).unwrap()
        } else {
            fixture.target_path.clone()
        };
        let snapshot = prepare_at(&fixture, &path, true).await;
        fixture.backend.block_next_pack_get();
        let cancel = CancellationToken::new();
        let operation = fixture
            .repository
            .operation(OperationKind::Blame, &cancel)
            .await
            .unwrap();
        let producer_snapshot = snapshot.clone();
        let producer_path = path.clone();
        let producer = tokio::spawn(async move {
            let result = producer_snapshot
                .shared_blame(&producer_path, &operation)
                .await;
            operation.finish(result).await
        });
        fixture.backend.wait_for_blocked_pack_get().await;
        let operation = fixture
            .repository
            .operation_with_limits(
                OperationKind::Blame,
                &cancel,
                OperationLimits {
                    max_blame_lines: if depth {
                        OperationLimits::default().max_blame_lines
                    } else {
                        1
                    },
                    max_depth: if depth {
                        1
                    } else {
                        OperationLimits::default().max_depth
                    },
                    ..OperationLimits::default()
                },
            )
            .await
            .unwrap();
        let result = snapshot.shared_blame(&path, &operation).await;
        let error = operation.finish(result).await.unwrap_err();
        assert!(
            matches!(error, Error::LimitExceeded { limit, maximum: 1, .. } if limit == if depth { "traversal depth" } else { "blame lines" })
        );
        fixture.backend.release_blocked_pack_get();
        assert!(producer.await.unwrap().is_ok());
        assert_eq!(metrics.0.load(Ordering::SeqCst), 1);
        fixture.runtime.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_blame_waiter_does_not_cancel_remaining_readers() {
    let metrics = Arc::new(Producers::default());
    let fixture = publish_with_metrics(
        DeltaKind::Ref,
        false,
        RepositoryOptions::default(),
        RuntimeOptions::default(),
        metrics.clone(),
    )
    .await;
    let snapshot = prepare(&fixture, false).await;
    fixture.backend.block_next_pack_get();
    let cancelled = CancellationToken::new();
    let mut tasks = Vec::new();
    for cancel in [cancelled.clone(), CancellationToken::new()] {
        let operation = fixture
            .repository
            .operation(OperationKind::Blame, &cancel)
            .await
            .unwrap();
        let snapshot = snapshot.clone();
        let path = fixture.target_path.clone();
        tasks.push(tokio::spawn(async move {
            let result = snapshot.shared_blame(&path, &operation).await;
            operation.finish(result).await
        }));
    }
    fixture.backend.wait_for_blocked_pack_get().await;
    wait_for_waiters(&fixture.runtime, 2).await;
    cancelled.cancel();
    let first = tasks.remove(0);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .unwrap()
            .unwrap(),
        Err(Error::Cancelled)
    ));
    fixture.backend.release_blocked_pack_get();
    assert!(tasks.remove(0).await.unwrap().is_ok());
    assert_eq!(metrics.0.load(Ordering::SeqCst), 1);
    fixture.runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn abandoned_last_blame_waiter_drains_producer_on_shutdown() {
    let fixture = publish(DeltaKind::Ref, false, RepositoryOptions::default()).await;
    let snapshot = prepare(&fixture, false).await;
    fixture.backend.block_next_pack_get();
    let cancel = CancellationToken::new();
    let operation = fixture
        .repository
        .operation(OperationKind::Blame, &cancel)
        .await
        .unwrap();
    let path = fixture.target_path.clone();
    let task = tokio::spawn(async move {
        let result = snapshot.shared_blame(&path, &operation).await;
        operation.finish(result).await
    });
    fixture.backend.wait_for_blocked_pack_get().await;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(5), fixture.runtime.shutdown())
        .await
        .expect("shutdown drains abandoned blame");
    let state = fixture.runtime.snapshot().await;
    assert_eq!(
        (
            state.active_blame_flights,
            state.active_blame_waiters,
            state.active_object_flights,
            fixture.backend.active_pack_gets.load(Ordering::SeqCst)
        ),
        (0, 0, 0, 0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn blame_deadline_detaches_only_the_expired_waiter() {
    let fixture = publish(DeltaKind::Ref, false, RepositoryOptions::default()).await;
    let snapshot = prepare(&fixture, false).await;
    fixture.backend.block_next_pack_get();
    let cancel = CancellationToken::new();
    let operation = fixture
        .repository
        .operation(OperationKind::Blame, &cancel)
        .await
        .unwrap();
    let survivor_snapshot = snapshot.clone();
    let path = fixture.target_path.clone();
    let survivor = tokio::spawn(async move {
        let result = survivor_snapshot.shared_blame(&path, &operation).await;
        operation.finish(result).await
    });
    fixture.backend.wait_for_blocked_pack_get().await;
    let operation = fixture
        .repository
        .operation_with_limits(
            OperationKind::Blame,
            &cancel,
            OperationLimits {
                max_duration: Duration::from_millis(200),
                ..OperationLimits::default()
            },
        )
        .await
        .unwrap();
    let path = fixture.target_path.clone();
    let expiring = tokio::spawn(async move {
        let result = snapshot.shared_blame(&path, &operation).await;
        operation.finish(result).await
    });
    wait_for_waiters(&fixture.runtime, 2).await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), expiring)
            .await
            .unwrap()
            .unwrap(),
        Err(Error::Timeout { .. })
    ));
    fixture.backend.release_blocked_pack_get();
    assert!(survivor.await.unwrap().is_ok());
    fixture.runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn different_object_limit_profiles_have_distinct_blame_producers() {
    let metrics = Arc::new(Producers::default());
    let fixture = publish_with_metrics(
        DeltaKind::Ref,
        false,
        RepositoryOptions::default(),
        RuntimeOptions::default(),
        metrics.clone(),
    )
    .await;
    let snapshot = prepare(&fixture, false).await;
    let cancel = CancellationToken::new();
    let options = RepositoryOptions::new(
        ObjectLimits {
            max_tag_depth: 31,
            ..ObjectLimits::default()
        },
        OperationLimits::default(),
    )
    .unwrap();
    let other = capture_fixture_repository(
        fixture.layout.clone(),
        fixture.repository.identity().clone(),
        fixture.runtime.clone(),
        options,
        &cancel,
    )
    .await
    .unwrap();
    let other_operation = other
        .operation(OperationKind::Blame, &cancel)
        .await
        .unwrap();
    let other_snapshot = other
        .snapshot(&Revision::Reference("main".into()), &other_operation)
        .await
        .unwrap();
    fixture.backend.block_next_pack_get();
    let mut tasks = Vec::new();
    for (snapshot, operation) in [
        (
            snapshot,
            fixture
                .repository
                .operation(OperationKind::Blame, &cancel)
                .await
                .unwrap(),
        ),
        (other_snapshot, other_operation),
    ] {
        let path = fixture.target_path.clone();
        tasks.push(tokio::spawn(async move {
            let result = snapshot.shared_blame(&path, &operation).await;
            operation.finish(result).await
        }));
    }
    fixture.backend.wait_for_blocked_pack_get().await;
    wait_for_waiters(&fixture.runtime, 2).await;
    assert_eq!(fixture.runtime.snapshot().await.active_blame_flights, 2);
    fixture.backend.release_blocked_pack_get();
    for task in tasks {
        assert!(task.await.unwrap().is_ok());
    }
    assert_eq!(metrics.0.load(Ordering::SeqCst), 2);
    fixture.runtime.shutdown().await;
}
