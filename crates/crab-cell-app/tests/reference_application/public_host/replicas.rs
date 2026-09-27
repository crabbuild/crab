//! Public host publication hints and reader shutdown.

use super::*;
use crate::reference_application::{performance_fixture, process_node};

struct ReaderHints(tokio::sync::mpsc::UnboundedSender<crab_cell_runtime::CellId>);

impl PeerRoundTrip for ReaderHints {
    fn send(
        &self,
        _: CellTarget,
        _: Vec<u8>,
        _: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        Box::pin(async { Err(Error::Peer("reader hint requires a selected node")) })
    }

    fn send_to_node(
        &self,
        target: CellTarget,
        _: NodeAdvertisement,
        _: Vec<u8>,
        _: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        let _ = self.0.send(target.cell_id());
        Box::pin(async move {
            use crab_cell_runtime::peer::{encode_peer_reply, wire};
            encode_peer_reply(&wire::PeerReply {
                outcome: Some(wire::peer_reply::Outcome::Read(wire::ReadReply {
                    receipt: Some(wire::Receipt {
                        cell_id: target.cell_id().as_bytes().to_vec(),
                        incarnation: vec![40; 16],
                        commit_sequence: 1,
                    }),
                    result: Some(wire::read_reply::Result::ReplicaReady(true)),
                })),
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn published_command_wakes_reader_recruitment_before_periodic_scan() {
    use crab_cell_runtime::{
        cell::application::ApplicationIdentity, node::lease::NodeLeaseGuard,
        peer::ReplicaPeerClient, read_policy::ReadPolicyStore,
    };
    use std::time::Duration;

    let application = Arc::new(compiled());
    let registry = application.registry();
    let tenant = TenantId::from_bytes([81; 16]);
    let app = ApplicationId::from_bytes([82; 16]);
    let root = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("publication-hints"),
        *app.as_bytes(),
    );
    let directory = process_node::directory(&layout, &registry);
    let now = now_ms();
    for node in 0..2 {
        directory
            .create(
                NodeAdvertisement::sign(
                    NodeId::from_bytes(*node_session(node).as_bytes()),
                    node_session(node),
                    format!("https://node-{node}.internal:8081"),
                    Digest::from_bytes([90; 32]),
                    Digest::from_bytes([94; 32]),
                    Digest::from_bytes([91; 32]),
                    registry.release_digest(),
                    &SigningKey::from_bytes(&[93; 32]),
                    1,
                    now,
                    now + 15_000,
                    registry.module_digests(),
                    vec![1],
                    NodeFailureDomain::default(),
                    NodeCapacity {
                        free_memory_bytes: 32 << 20,
                        free_disk_bytes: 64 << 20,
                        job_credits: 4,
                        ..Default::default()
                    },
                )
                .unwrap(),
                now,
            )
            .await
            .unwrap();
    }
    let node = crab_cell_host::CellNodeBuilder::new(application)
        .with_runtime(SqlWorkerPool::new(2, 8).unwrap(), 64 << 20)
        .with_session(node_session(0))
        .with_replica_host(reference_host())
        .build()
        .unwrap();
    node.install_task_group(
        tokio_util::sync::CancellationToken::new(),
        tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    node.install_node_lease_for_startup(NodeLeaseGuard::new(now, now + 15_000).unwrap())
        .unwrap();
    let handle = bootstrap_reference_cell(
        &node.runtime(),
        &registry,
        &layout,
        &root,
        tenant,
        app,
        node_session(0),
        SQL_NAMESPACE,
        CatalogRole::Sql,
        SQL_MODULE,
        40,
        performance_fixture::install_sql_tables,
    )
    .await
    .unwrap();
    let target = CellTarget::new(tenant, app, SQL_NAMESPACE, &partition_for_shard(0)).unwrap();
    ReadPolicyStore::new(layout.clone())
        .create(target.cell_id(), IncarnationId::from_bytes([40; 16]), 1)
        .await
        .unwrap();
    node.install_read_replicas(
        layout,
        directory,
        root.path().join("readers"),
        Limits::default(),
    )
    .unwrap();
    let (sent, mut hints) = tokio::sync::mpsc::unbounded_channel();
    node.install_read_replica_recruitment(
        ApplicationIdentity::new(tenant, app),
        ReplicaPeerClient::new(
            registry.clone(),
            Arc::new(PeerSigner::new(
                node_session(0),
                registry.release_digest(),
                SigningKey::from_bytes(&[93; 32]),
            )),
            PeerPrincipal {
                issuer: "reference-runtime".into(),
                subject: "owner".into(),
                actions: vec!["cell.replica.activate".into()],
            },
            Arc::new(ReaderHints(sent)),
        ),
    )
    .unwrap();
    node.start().unwrap();
    // Consume the immediate periodic pass before publishing. The next tick is
    // five seconds away, so only a publication wake-up can satisfy this bound.
    tokio::time::timeout(Duration::from_secs(2), hints.recv())
        .await
        .unwrap()
        .unwrap();
    let client = CellClient::local(registry, handle);
    let typed = node
        .application_handle::<ReferenceApplication>(client, tenant, app)
        .unwrap();
    let generated = ReferenceClient::new(typed).unwrap();
    generated
        .orders(&OrderId(b"publication-hints".to_vec()))
        .unwrap()
        .receive_cron(identity(108, 0, 0), invocation(1))
        .await
        .unwrap();
    let notified = tokio::time::timeout(Duration::from_secs(2), hints.recv()).await;
    node.shutdown().await.unwrap();
    assert!(
        matches!(notified, Ok(Some(cell)) if cell == target.cell_id()),
        "published command waited for the periodic reader scan: {notified:?}"
    );
}

#[tokio::test]
async fn public_host_drain_cancels_reader_activation_waiting_on_storage() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    use std::{task::Poll, time::Duration};

    let application = Arc::new(compiled());
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(ThrottledStore::new(
            InMemory::new(),
            ThrottleConfig {
                wait_get_per_call: Duration::from_secs(3600),
                ..Default::default()
            },
        ))),
        Path::from("blocked-reader"),
        [82; 16],
    );
    let root = tempfile::TempDir::new().unwrap();
    let node = crab_cell_host::CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 32).unwrap(), 64 << 20)
        .with_replica_host(reference_host())
        .with_session(node_session(1))
        .build()
        .unwrap();
    node.install_task_group(
        tokio_util::sync::CancellationToken::new(),
        tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    let manager = node
        .install_read_replicas(
            layout.clone(),
            process_node::directory(&layout, &application.registry()),
            root.path().to_owned(),
            Limits::default(),
        )
        .unwrap();
    let target = CellTarget::new(
        TenantId::from_bytes([81; 16]),
        ApplicationId::from_bytes([82; 16]),
        SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .unwrap();
    let recruiter = node
        .install_read_replica_recruitment(
            crab_cell_runtime::cell::application::ApplicationIdentity::new(
                target.tenant(),
                target.application(),
            ),
            crab_cell_runtime::peer::ReplicaPeerClient::new(
                application.registry(),
                Arc::new(PeerSigner::new(
                    node_session(1),
                    application.registry().release_digest(),
                    SigningKey::from_bytes(&[93; 32]),
                )),
                crab_cell_runtime::peer::PeerPrincipal {
                    issuer: "reference-runtime".into(),
                    subject: "drain".into(),
                    actions: vec!["cell.replica.activate".into()],
                },
                Arc::new(process_node::EnrolledReplicaTransport),
            ),
        )
        .unwrap();
    let recruitment = recruiter.reconcile(target.clone());
    tokio::pin!(recruitment);
    std::future::poll_fn(|cx| {
        assert!(recruitment.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let activation = manager.activate(target.clone(), node_session(0));
    tokio::pin!(activation);
    // Poll into the provider delay while activation owns its lane; no sleep
    // or scheduler timing assumption is needed to put shutdown behind it.
    std::future::poll_fn(|cx| {
        assert!(activation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let (activated, recruited, drained) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(&mut activation, &mut recruitment, node.shutdown())
    })
    .await
    .expect("reader drain waited for stalled storage");
    assert!(matches!(activated, Err(Error::RuntimeClosed)));
    drained.unwrap();
    assert!(matches!(recruited, Err(Error::RuntimeClosed)));
    assert!(matches!(
        recruiter.reconcile_active().await,
        Err(Error::RuntimeClosed)
    ));
    assert!(matches!(
        manager.activate(target, node_session(0)).await,
        Err(Error::RuntimeClosed)
    ));
}
