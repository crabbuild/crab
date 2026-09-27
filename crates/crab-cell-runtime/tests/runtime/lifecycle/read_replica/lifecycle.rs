//! In-flight snapshot work at the node drain and schema boundaries.

use super::*;

pub(super) struct ReadLogicalTime;

impl Query for ReadLogicalTime {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = ();
    type Output = i64;

    fn execute(context: &mut QueryContext<'_>, _: ()) -> crab_cell_runtime::Result<i64> {
        Ok(context.now_ms())
    }
}

#[tokio::test]
async fn owner_and_replica_queries_do_not_precede_committed_time() {
    let fixture = fixture();
    let owner = SessionId::from_bytes([44; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 << 20, owner).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, owner).await;
    let committed_time = now_ms() + 10_000;
    handle
        .execute(
            crate::support::fixtures::mutation_identity(46),
            Digest::from_bytes([47; 32]),
            committed_time,
            64,
            64,
            |_| Ok(HandlerOutcome::Success(Vec::new())),
        )
        .await
        .unwrap();
    let registry = compiled_reader_registry();
    let directory = owner_directory(&fixture, owner, &registry).await;
    let reader_runtime = CellRuntime::new(
        SqlWorkerPool::new(2, 512).unwrap(),
        8 << 20,
        SessionId::from_bytes([45; 16]),
    )
    .unwrap();
    let reader = CellReadReplica::open(
        reader_runtime.clone(),
        Arc::clone(&registry),
        CellAuthority::new(fixture.layout.clone()),
        directory,
        fixture.replica.clone(),
        fixture.target.clone(),
        &fixture._directory.path().join("clock-reader.sqlite"),
    )
    .await
    .unwrap();
    let local = CellClient::local(registry, handle.clone());
    let local_time = local
        .query::<ReadLogicalTime>(&fixture.target, None, ())
        .await
        .unwrap()
        .output;
    let replica_time = reader
        .query::<ReadLogicalTime>(None, ())
        .await
        .unwrap()
        .output;
    assert_eq!((local_time, replica_time), (committed_time, committed_time));
    drop(reader);
    reader_runtime.shutdown().await.unwrap();
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_routing_overlaps_independent_control_and_policy_reads() {
    let store = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = reads.clone();
    let fixture = fixture_with_limits_and_store(
        b"parallel-reader-routing",
        Limits::default(),
        Store::new(store.clone()).with_read_request_observer(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::Relaxed);
        })),
    );
    let registry = compiled_reader_registry();
    let directory = NodeDirectory::new(
        fixture.layout.clone(),
        Digest::from_bytes([9; 32]),
        Digest::from_bytes([10; 32]),
        registry.release_digest(),
    );
    let router = ReplicaReadRouter::new(CellAuthority::new(fixture.layout.clone()), directory);
    reads.store(0, Ordering::Relaxed);
    store.arm_gets();
    let selected = router.selected(&fixture.target);
    tokio::pin!(selected);
    assert!(futures_util::poll!(selected.as_mut()).is_pending());
    let started = reads.load(Ordering::Relaxed);
    store.release_gets();
    assert!(matches!(
        selected.await,
        Err(crab_cell_runtime::Error::ReplicaUnavailable)
    ));
    assert_eq!(
        started, 2,
        "policy discovery waited for the blocked control read"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_refreshes_coalesce_after_the_first_authority_read() {
    let store = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let fixture = fixture_with_limits_and_store(
        b"coalesced-reader-refresh",
        Limits::default(),
        Store::new(store.clone()),
    );
    let owner = SessionId::from_bytes([44; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 << 20, owner).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, owner).await;
    let registry = compiled_reader_registry();
    let directory = owner_directory(&fixture, owner, &registry).await;
    let reads = Arc::new(ControlReads::default());
    let authority = CellAuthority::with_telemetry(
        fixture.layout.clone(),
        crab_cell_runtime::fleet::telemetry::CellTelemetryHandle::from_sink(reads.clone()),
    );
    let reader_runtime = CellRuntime::new(
        SqlWorkerPool::new(2, 512).unwrap(),
        8 << 20,
        SessionId::from_bytes([45; 16]),
    )
    .unwrap();
    let original = fixture._directory.path().join("original.sqlite");
    let reader = CellReadReplica::open(
        reader_runtime.clone(),
        registry,
        authority,
        directory,
        fixture.replica.clone(),
        fixture.target.clone(),
        &original,
    )
    .await
    .unwrap();
    let committed = handle
        .execute(
            crate::support::fixtures::mutation_identity(46),
            Digest::from_bytes([47; 32]),
            now_ms(),
            64,
            64,
            |transaction| {
                transaction.execute("UPDATE counter SET value = 1", [])?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();
    store.arm_gets();
    let first_path = fixture._directory.path().join("first.sqlite");
    let second_path = fixture._directory.path().join("second.sqlite");
    let first_reader = reader.clone();
    let destination = first_path.clone();
    let first = tokio::spawn(async move { first_reader.refresh(&destination).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        store.wait_until_get_blocked(),
    )
    .await
    .unwrap();
    let before = reads.0.load(Ordering::Relaxed);
    let (first, second, pending, reads_while_blocked) = {
        let second = reader.refresh(&second_path);
        tokio::pin!(second);
        let pending = futures_util::poll!(second.as_mut()).is_pending();
        let reads_while_blocked = reads.0.load(Ordering::Relaxed) - before;
        // Release the injected I/O stall before assertions so a failed
        // coalescing invariant cannot leave a runtime task parked forever.
        store.release_gets();
        let (first, second) = tokio::join!(first, second);
        (
            first.unwrap().unwrap(),
            second.unwrap(),
            pending,
            reads_while_blocked,
        )
    };
    assert!(pending);
    assert_eq!(reads_while_blocked, 0);
    assert_eq!(first, second);
    assert_eq!(first.commit_sequence, committed.commit_sequence());
    assert!(!original.exists());
    assert!(first_path.exists());
    assert!(!second_path.exists());
    assert_eq!(reader_runtime.stats().resident_bytes(), 12 << 20);
    assert_eq!(
        reader.query::<ReadCounter>(None, 0).await.unwrap().output,
        1
    );
    reader.close();
    drop(reader);
    assert!(!first_path.exists());
    assert_eq!(reader_runtime.stats().resident_bytes(), 0);
    reader_runtime.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_change_fences_an_inflight_old_snapshot_query() {
    exercise_schema_change(fixture_for(b"read-replica-migration")).await;
}

pub(super) async fn exercise_schema_change(fixture: Fixture) {
    let session = SessionId::from_bytes([34; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 << 20, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    let registry = compiled_reader_registry();
    let directory = owner_directory(&fixture, session, &registry).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let reader_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 512).unwrap(),
        8 << 20,
        SessionId::from_bytes([35; 16]),
    )
    .unwrap();
    let old_path = fixture._directory.path().join("old-schema.sqlite");
    let reader = CellReadReplica::open(
        reader_runtime.clone(),
        Arc::clone(&registry),
        authority.clone(),
        directory.clone(),
        fixture.replica.clone(),
        fixture.target.clone(),
        &old_path,
    )
    .await
    .unwrap();
    let before = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let pause = QueryPause::new();
    let pause_id = pause.id;
    let pending_reader = reader.clone();
    let pending =
        tokio::spawn(async move { pending_reader.query::<ReadCounter>(None, pause_id).await });
    pause.entered().await;
    let plan = registry
        .next_migration(NAMESPACE, handle.code(), handle.schema())
        .unwrap()
        .unwrap();
    let migrated = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        handle.migrate(plan, now_ms()),
    )
    .await;
    pause.release();
    let migrated = migrated.unwrap().unwrap();
    assert!(matches!(
        pending.await.unwrap(),
        Err(crab_cell_runtime::Error::Fenced)
    ));
    let after = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.value().schema, 2);
    assert_eq!(after.value().code, registry.module_code(MODULE).unwrap());
    assert_eq!(after.value().owner, before.value().owner);
    assert_eq!(after.value().epoch, before.value().epoch);
    assert!(
        after.value().root.as_ref().unwrap().commit_sequence
            > before.value().root.as_ref().unwrap().commit_sequence
    );
    let rejected_path = fixture._directory.path().join("stale-refresh.sqlite");
    assert!(matches!(
        reader.refresh(&rejected_path).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    assert!(!rejected_path.exists());
    let new_path = fixture._directory.path().join("new-schema.sqlite");
    let replacement = CellReadReplica::open(
        reader_runtime.clone(),
        registry,
        authority,
        directory,
        fixture.replica,
        fixture.target,
        &new_path,
    )
    .await
    .unwrap();
    assert_eq!(
        replacement
            .query::<ReadCounter>(None, 0)
            .await
            .unwrap()
            .output,
        10
    );
    reader.close();
    replacement.close();
    drop(reader);
    drop(replacement);
    assert!(!old_path.exists());
    assert!(!new_path.exists());
    migrated.handle.drain().await.unwrap();
    reader_runtime.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}

pub(super) async fn drain_waits_for_replica_sql(
    fixture: &Fixture,
    registry: &Arc<crab_cell_runtime::registry::Registry>,
    directory: &NodeDirectory,
) {
    for cancel in [false, true] {
        drain_query(fixture, registry, directory, cancel).await;
    }
}

async fn drain_query(
    fixture: &Fixture,
    registry: &Arc<crab_cell_runtime::registry::Registry>,
    directory: &NodeDirectory,
    cancel: bool,
) {
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 256).unwrap(),
        8 << 20,
        SessionId::from_bytes([33; 16]),
    )
    .unwrap();
    let path = fixture._directory.path().join("draining-reader.sqlite");
    let reader = CellReadReplica::open(
        runtime.clone(),
        Arc::clone(registry),
        CellAuthority::new(fixture.layout.clone()),
        directory.clone(),
        fixture.replica.clone(),
        fixture.target.clone(),
        &path,
    )
    .await
    .unwrap();
    let pause = QueryPause::new();
    let pause_id = pause.id;
    let pending_reader = reader.clone();
    let mut pending =
        tokio::spawn(async move { pending_reader.query::<ReadCounter>(None, pause_id).await });
    pause.entered().await;
    reader.close();
    drop(reader);
    let cancelled = if cancel {
        pending.abort();
        Some((&mut pending).await.unwrap_err())
    } else {
        None
    };
    let retained_while_running = runtime.stats().resident_bytes();
    let descriptors_while_running = runtime.stats().file_descriptors();
    let draining = runtime.clone();
    let mut shutdown = tokio::spawn(async move { draining.shutdown().await });
    let early = tokio::time::timeout(std::time::Duration::from_millis(50), &mut shutdown)
        .await
        .ok();
    let completed_early = early.is_some();
    let jobs_during_drain = runtime.stats().worker_jobs();
    // Always release the SQL callback before asserting the drain result, so a
    // regression cannot strand a blocking worker and hang the test process.
    pause.release();
    let output = match cancelled {
        Some(error) => {
            assert!(error.is_cancelled());
            None
        }
        None => Some(pending.await.unwrap()),
    };
    match early {
        Some(result) => result.unwrap().unwrap(),
        None => shutdown.await.unwrap().unwrap(),
    }
    assert!(matches!(
        output,
        None | Some(Err(crab_cell_runtime::Error::Fenced))
    ));
    assert_eq!(retained_while_running, 12 << 20);
    assert_eq!(descriptors_while_running, 4);
    assert_eq!(jobs_during_drain, 1);
    assert_eq!(runtime.stats().worker_jobs(), 0);
    assert_eq!(runtime.stats().resident_bytes(), 0);
    assert!(!path.exists());
    assert!(
        !completed_early,
        "node drain returned while replica SQL was running"
    );
}

struct StalledRead(Arc<AtomicUsize>);

impl Drop for StalledRead {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl PeerReplicaResolver for StalledRead {
    fn resolve(
        &self,
        _: CellTarget,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = crab_cell_runtime::Result<CellReadReplica>>
                + Send
                + 'static,
        >,
    > {
        let guard = Self(self.0.clone());
        Box::pin(async move {
            let _guard = guard;
            std::future::pending().await
        })
    }
}

struct StalledPeer {
    session: Option<SessionId>,
    dropped: Arc<AtomicUsize>,
    healthy: LoopbackReplica,
}

impl PeerRoundTrip for StalledPeer {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>,
    > {
        self.healthy.send(target, request, remaining_ms)
    }

    fn send_to_node(
        &self,
        target: CellTarget,
        node: NodeAdvertisement,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>,
    > {
        if Some(node.session()) != self.session {
            return self
                .healthy
                .send_to_node(target, node, request, remaining_ms);
        }
        let guard = StalledRead(self.dropped.clone());
        Box::pin(async move {
            let _guard = guard;
            std::future::pending().await
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_remote_reader_leaves_time_for_a_healthy_replica() {
    stalled_reader_is_skipped(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_local_resolver_leaves_time_for_a_healthy_replica() {
    stalled_reader_is_skipped(true).await;
}

async fn stalled_reader_is_skipped(local: bool) {
    let fixture = fixture();
    let owner = SessionId::from_bytes([44; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 << 20, owner).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, owner).await;
    let registry = compiled_reader_registry();
    let directory = owner_directory(&fixture, owner, &registry).await;
    for node in [14, 15] {
        let now = now_ms();
        directory
            .create(
                NodeAdvertisement::sign(
                    NodeId::from_bytes([node; 16]),
                    SessionId::from_bytes([node; 16]),
                    format!("https://reader-{node}.internal:8081"),
                    directory.fleet(),
                    Digest::from_bytes([12; 32]),
                    Digest::from_bytes([10; 32]),
                    registry.release_digest(),
                    &ed25519_dalek::SigningKey::from_bytes(&[node; 32]),
                    1,
                    now,
                    now + 15_000,
                    vec![CODE],
                    vec![1],
                    NodeFailureDomain::default(),
                    NodeCapacity {
                        free_memory_bytes: 32 << 20,
                        free_disk_bytes: 1 << 20,
                        job_credits: 1,
                        ..NodeCapacity::default()
                    },
                )
                .unwrap(),
                now,
            )
            .await
            .unwrap();
    }
    let reader_runtime = CellRuntime::new(
        SqlWorkerPool::new(2, 2)
            .unwrap()
            .with_native_memory_limit(32 << 20)
            .unwrap(),
        8 << 20,
        SessionId::from_bytes([14; 16]),
    )
    .unwrap();
    let reader = CellReadReplica::open(
        reader_runtime.clone(),
        registry.clone(),
        CellAuthority::new(fixture.layout.clone()),
        directory.clone(),
        fixture.replica.clone(),
        fixture.target.clone(),
        &fixture._directory.path().join("routing-reader.sqlite"),
    )
    .await
    .unwrap();
    let receipt = reader.receipt().await;
    crab_cell_runtime::read_policy::ReadPolicyStore::new(fixture.layout.clone())
        .create(fixture.target.cell_id(), receipt.incarnation, 2)
        .await
        .unwrap();
    let router = ReplicaReadRouter::new(CellAuthority::new(fixture.layout.clone()), directory);
    let (_, selected) = router.selected(&fixture.target).await.unwrap();
    assert_eq!(selected.len(), 2);
    let blocked = selected[0].session();
    let dropped = Arc::new(AtomicUsize::new(0));
    let local_resolver = StalledRead(dropped.clone());
    let peer_session = SessionId::from_bytes([21; 16]);
    let key = ed25519_dalek::SigningKey::from_bytes(&[22; 32]);
    let dispatcher = PeerDispatcher::new(
        registry.clone(),
        Arc::new(NoOwner),
        Arc::new(ReadAuthorizer),
    )
    .with_replica_resolver(Arc::new(ReplicaResolver(reader.clone())));
    let transport = StalledPeer {
        session: (!local).then_some(blocked),
        dropped: dropped.clone(),
        healthy: LoopbackReplica {
            verifier: Arc::new(PeerVerifier::new(
                peer_session,
                registry.release_digest(),
                key.verifying_key(),
            )),
            dispatcher: Arc::new(dispatcher),
        },
    };
    let peer = ReplicaPeerClient::new(
        registry.clone(),
        Arc::new(PeerSigner::new(
            peer_session,
            registry.release_digest(),
            key,
        )),
        PeerPrincipal {
            issuer: "test".into(),
            subject: "reader".into(),
            actions: vec!["repository.read".into()],
        },
        Arc::new(transport),
    );
    let queried = router
        .query::<ReadCounter>(
            &peer,
            local.then_some((blocked, &local_resolver as &dyn PeerReplicaResolver)),
            &fixture.target,
            Some(receipt),
            0,
        )
        .await;
    // Drain even on the red run, so a failed routing assertion cannot strand SQL workers.
    drop(peer);
    drop(reader);
    reader_runtime.shutdown().await.unwrap();
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
    let (observed, served_by) = queried.unwrap();
    assert_eq!(
        (observed.output, observed.receipt, served_by),
        (0, receipt, selected[1].node())
    );
    assert_eq!(
        dropped.load(Ordering::Relaxed),
        1,
        "stalled attempt was not cancelled"
    );
}
