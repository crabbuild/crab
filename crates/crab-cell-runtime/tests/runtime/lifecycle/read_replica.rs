//! Read-only Cell snapshots and their authority response gate.

use super::*;
use std::sync::{Barrier, OnceLock};

use crab_cell_runtime::cell::actor::CellHandle;
use crab_cell_runtime::client::CellReadReplica;
use crab_cell_runtime::client::{
    CellClient, CellDescription, InvocationError, ReadPolicy, ReplicaReadRouter,
};
use crab_cell_runtime::node::{NodeAdvertisement, NodeCapacity, NodeDirectory, NodeFailureDomain};
use crab_cell_runtime::peer::{
    PeerAuthorizer, PeerCellResolver, PeerDispatcher, PeerPrincipal, PeerReplicaResolver,
    PeerRoundTrip, PeerSigner, PeerVerifier, ReplicaPeerClient, VerifiedPeerRequest,
};
use crab_cell_runtime::primitives::sql::{SqlBatch, SqlStatement, SqlValue};
use crab_cell_runtime::registry::{
    BuildDescriptor, CellModule, MigrationDescriptor, ModuleDescriptor, NamespaceDescriptor,
    OperationDescriptor, Query, QueryContext, Registry, RegistryBuilder, RetainedCodeDescriptor,
};

mod lifecycle;

const MODULE: &str = "replica-counter";
const SCHEMA: &str = "CREATE TABLE counter(value INTEGER NOT NULL)";
const MIGRATED_SCHEMA: &str =
    "ALTER TABLE counter ADD COLUMN label TEXT; UPDATE counter SET value = value + 10";
const CODE: Digest = Digest::from_bytes([5; 32]);
const NAMESPACE: NamespaceId = NamespaceId::from_bytes([6; 16]);
static QUERY_BARRIERS: OnceLock<(Barrier, Barrier)> = OnceLock::new();

#[derive(Default)]
struct ControlReads(AtomicUsize);

impl crab_cell_runtime::fleet::telemetry::CellTelemetry for ControlReads {
    fn control_read(&self, _: std::time::Duration, _: bool) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct ReplicaResolver(CellReadReplica);

impl PeerReplicaResolver for ReplicaResolver {
    fn resolve(
        &self,
        _target: CellTarget,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = crab_cell_runtime::Result<CellReadReplica>>
                + Send
                + 'static,
        >,
    > {
        let reader = self.0.clone();
        Box::pin(async move { Ok(reader) })
    }
}

struct NoOwner;

impl PeerCellResolver for NoOwner {
    fn resolve(
        &self,
        _target: CellTarget,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = crab_cell_runtime::Result<CellHandle>>
                + Send
                + 'static,
        >,
    > {
        Box::pin(async { Err(crab_cell_runtime::Error::CellNotActive) })
    }
}

struct ReadAuthorizer;

impl PeerAuthorizer for ReadAuthorizer {
    fn authorize(&self, request: &VerifiedPeerRequest) -> crab_cell_runtime::Result<()> {
        if request.permits("repository.read") {
            Ok(())
        } else {
            Err(crab_cell_runtime::Error::PeerAuthorization("read denied"))
        }
    }
}

struct LoopbackReplica {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
}

impl PeerRoundTrip for LoopbackReplica {
    fn send(
        &self,
        _target: CellTarget,
        _request: Vec<u8>,
        _remaining_ms: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>,
    > {
        Box::pin(async { Err(crab_cell_runtime::Error::Peer("owner route unavailable")) })
    }

    fn send_to_node(
        &self,
        _target: CellTarget,
        _node: NodeAdvertisement,
        request: Vec<u8>,
        _remaining_ms: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>,
    > {
        let verifier = Arc::clone(&self.verifier);
        let dispatcher = Arc::clone(&self.dispatcher);
        Box::pin(async move {
            let now = now_ms();
            let verified = verifier.verify(&request, now)?;
            dispatcher.dispatch_bytes(&verified, now).await
        })
    }
}

fn query_barriers() -> &'static (Barrier, Barrier) {
    QUERY_BARRIERS.get_or_init(|| (Barrier::new(2), Barrier::new(2)))
}

struct CounterModule;

impl CellModule for CounterModule {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: MODULE,
            source_digest: Digest::from_bytes([7; 32]),
            retained_codes: &[RetainedCodeDescriptor {
                code: CODE,
                schema_min: 1,
                schema_max: 2,
            }],
            schema_min: 1,
            schema_max: 2,
            migrations: Box::leak(Box::new([
                MigrationDescriptor {
                    version: 1,
                    sql: SCHEMA,
                    digest: Digest::from_bytes(*blake3::hash(SCHEMA.as_bytes()).as_bytes()),
                },
                MigrationDescriptor {
                    version: 2,
                    sql: MIGRATED_SCHEMA,
                    digest: Digest::from_bytes(
                        *blake3::hash(MIGRATED_SCHEMA.as_bytes()).as_bytes(),
                    ),
                },
            ])),
            commands: &[],
            queries: &[OperationDescriptor {
                id: 1,
                codec_version: 1,
                schema_min: 1,
                schema_max: 2,
                input_limit: 16,
                output_limit: 16,
            }],
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &[NamespaceDescriptor {
                id: NAMESPACE,
                name: MODULE,
                role: CatalogRole::Repository,
                shards: 1,
                effect_targets: &[],
                dead_letter: None,
            }],
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        registry.bind_query::<ReadCounter>()
    }
}

struct ReadCounter;

impl Query for ReadCounter {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = u64;
    type Output = u64;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        if input == 99 {
            let (entered, release) = query_barriers();
            entered.wait();
            release.wait();
        } else if input == 98 {
            let (entered, release) = lifecycle::migration_query_barriers();
            entered.wait();
            release.wait();
        }
        let result = context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "SELECT value FROM counter".into(),
                parameters: vec![],
            }],
        })?;
        match result.first().and_then(|set| set.rows.first()) {
            Some(row) if matches!(row.first(), Some(SqlValue::Integer(_))) => {
                let Some(SqlValue::Integer(value)) = row.first() else {
                    return Err(crab_cell_runtime::Error::Command("counter row is invalid"));
                };
                u64::try_from(*value)
                    .map_err(|_| crab_cell_runtime::Error::Command("counter is negative"))
            }
            _ => Err(crab_cell_runtime::Error::Command("counter row is missing")),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replica_reads_exact_snapshot_and_fences_after_release() {
    let fixture = fixture_for(b"read-replica");
    exercise_replica_read(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated pre-created RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_replica_reads_exact_root_and_policy_cas() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let store = build_explicit_store(
        &required("CRAB_CELL_TEST_BUCKET"),
        ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_CELL_TEST_ENDPOINT")),
        true,
    )
    .unwrap();
    let prefix = Path::from(required("CRAB_CELL_TEST_PREFIX"));
    let fixture = fixture_with_limits_and_store_at_prefix(
        b"read-replica",
        Limits::default(),
        store.clone(),
        prefix.clone(),
    );
    exercise_replica_read(&fixture).await;
    let policy = crab_cell_runtime::read_policy::ReadPolicyStore::new(fixture.layout.clone());
    let first = policy
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        policy
            .update(&first, 2)
            .await
            .unwrap()
            .value()
            .desired_readers(),
        2
    );
    lifecycle::exercise_schema_change(fixture_with_limits_and_store_at_prefix(
        b"read-replica-migration",
        Limits::default(),
        store,
        prefix,
    ))
    .await;
}

fn compiled_reader_registry() -> Arc<Registry> {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: "replica-test".into(),
        cargo_lock_digest: Digest::from_bytes([8; 32]),
    });
    builder.register(CounterModule).unwrap();
    Arc::new(builder.finish().unwrap())
}

async fn owner_directory(
    fixture: &Fixture,
    session: SessionId,
    registry: &Registry,
) -> NodeDirectory {
    let fleet = Digest::from_bytes([9; 32]);
    let image = Digest::from_bytes([10; 32]);
    let release = registry.release_digest();
    let directory = NodeDirectory::new(fixture.layout.clone(), fleet, image, release);
    let now = now_ms();
    directory
        .create(
            NodeAdvertisement::sign(
                NodeId::from_bytes([11; 16]),
                session,
                "https://node.internal:8081".into(),
                fleet,
                Digest::from_bytes([12; 32]),
                image,
                release,
                &ed25519_dalek::SigningKey::from_bytes(&[13; 32]),
                1,
                now,
                now + 15_000,
                registry.module_digests(),
                vec![1],
                NodeFailureDomain::default(),
                NodeCapacity {
                    free_memory_bytes: 1 << 20,
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

    directory
}

async fn exercise_replica_read(fixture: &Fixture) {
    let session = SessionId::from_bytes([4; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 << 20, session).unwrap();
    // Hold one old-view query while a second admitted SQL job opens its replacement.
    let reader_runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(2, 512).unwrap(),
        8 << 20,
        SessionId::from_bytes([14; 16]),
        crab_ltx::Host::default().with_local_disk_budget(crab_ltx::DiskBudget::new(8 << 20)),
    )
    .unwrap();
    let handle = bootstrap_on(&runtime, fixture, session).await;
    let active = runtime.active_catalog_entries().await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].cell(), fixture.target.cell_id());

    let registry = compiled_reader_registry();
    let fleet = Digest::from_bytes([9; 32]);
    let image = Digest::from_bytes([10; 32]);
    let release = registry.release_digest();
    let directory = owner_directory(fixture, session, &registry).await;

    let reader_path = fixture._directory.path().join("reader.sqlite");
    let constrained = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        8 << 20,
        SessionId::from_bytes([15; 16]),
    )
    .unwrap();
    assert!(matches!(
        CellReadReplica::open(
            constrained.clone(),
            Arc::clone(&registry),
            CellAuthority::new(fixture.layout.clone()),
            directory.clone(),
            fixture.replica.clone(),
            fixture.target.clone(),
            &reader_path,
        )
        .await,
        Err(crab_cell_runtime::Error::Capacity(_))
    ));
    assert!(!reader_path.exists());
    constrained.shutdown().await.unwrap();
    let reader = CellReadReplica::open(
        reader_runtime.clone(),
        Arc::clone(&registry),
        CellAuthority::new(fixture.layout.clone()),
        directory.clone(),
        fixture.replica.clone(),
        fixture.target.clone(),
        &reader_path,
    )
    .await
    .unwrap();
    assert_eq!(reader_runtime.stats().file_descriptors(), 4);
    assert_eq!(reader_runtime.stats().resident_bytes(), 12 << 20);
    let disk_bytes = reader_runtime.stats().local_disk_reserved_bytes();
    assert_eq!(disk_bytes, 0);
    assert_eq!(
        reader.query::<ReadCounter>(None, 0).await.unwrap().output,
        0
    );
    let authority = CellAuthority::new(fixture.layout.clone());
    let control = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let expected = CellDescription {
        cell: fixture.target.cell_id(),
        incarnation: control.value().incarnation,
        code: control.value().code,
        schema: control.value().schema,
    };
    let peer_session = SessionId::from_bytes([21; 16]);
    let peer_key = ed25519_dalek::SigningKey::from_bytes(&[22; 32]);
    let dispatcher = PeerDispatcher::new(
        Arc::clone(&registry),
        Arc::new(NoOwner),
        Arc::new(ReadAuthorizer),
    )
    .with_replica_resolver(Arc::new(ReplicaResolver(reader.clone())));
    let transport = LoopbackReplica {
        verifier: Arc::new(PeerVerifier::new(
            peer_session,
            release,
            peer_key.verifying_key(),
        )),
        dispatcher: Arc::new(dispatcher),
    };
    let peer_client = ReplicaPeerClient::new(
        Arc::clone(&registry),
        Arc::new(PeerSigner::new(peer_session, release, peer_key)),
        PeerPrincipal {
            issuer: "test".into(),
            subject: "reader".into(),
            actions: vec!["repository.read".into()],
        },
        Arc::new(transport),
    );
    let reader_node = NodeAdvertisement::sign(
        NodeId::from_bytes([23; 16]),
        SessionId::from_bytes([14; 16]),
        "https://reader.internal:8081".into(),
        fleet,
        Digest::from_bytes([24; 32]),
        image,
        release,
        &ed25519_dalek::SigningKey::from_bytes(&[25; 32]),
        1,
        now_ms(),
        now_ms() + 15_000,
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
    .unwrap();
    assert_eq!(
        peer_client
            .query::<ReadCounter>(&fixture.target, reader_node.clone(), expected, None, 0)
            .await
            .unwrap()
            .output,
        0
    );
    directory
        .create(reader_node.clone(), now_ms())
        .await
        .unwrap();
    let policy = crab_cell_runtime::read_policy::ReadPolicyStore::new(fixture.layout.clone());
    let selected_policy = policy
        .create(fixture.target.cell_id(), expected.incarnation, 1)
        .await
        .unwrap();
    let control_reads = Arc::new(ControlReads::default());
    let routing_authority = CellAuthority::with_telemetry(
        fixture.layout.clone(),
        crab_cell_runtime::fleet::telemetry::CellTelemetryHandle::from_sink(control_reads.clone()),
    );
    let router = ReplicaReadRouter::new(routing_authority, directory.clone());
    let (_, selected) = router.selected(&fixture.target).await.unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(control_reads.0.load(Ordering::Relaxed), 1);
    let owner_client = CellClient::local(Arc::clone(&registry), handle.clone());
    let configured = owner_client
        .with_read_replicas(router.clone(), peer_client.clone(), None)
        .unwrap();
    let replica_client = configured.with_read_policy(ReadPolicy::Replica);
    assert_eq!(
        replica_client
            .query::<ReadCounter>(&fixture.target, None, 0)
            .await
            .unwrap()
            .output,
        0
    );
    let local_client = owner_client
        .with_read_replicas(
            router,
            peer_client.clone(),
            Some((
                reader_node.session(),
                Arc::new(ReplicaResolver(reader.clone())),
            )),
        )
        .unwrap()
        .with_read_policy(ReadPolicy::Replica);
    assert_eq!(
        local_client
            .query::<ReadCounter>(&fixture.target, None, 0)
            .await
            .unwrap()
            .output,
        0
    );
    let committed = handle
        .execute(
            crate::support::fixtures::mutation_identity(91),
            Digest::from_bytes([92; 32]),
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
    assert!(committed.commit_sequence() > reader.receipt().await.commit_sequence);
    assert_eq!(
        reader.query::<ReadCounter>(None, 0).await.unwrap().output,
        0
    );
    let minimum = crab_cell_runtime::Receipt {
        commit_sequence: committed.commit_sequence(),
        ..reader.receipt().await
    };
    assert!(matches!(
        reader.query::<ReadCounter>(Some(minimum), 0).await,
        Err(crab_cell_runtime::Error::ReplicaBehind { observed_sequence, minimum_sequence })
            if observed_sequence < minimum_sequence && minimum_sequence == minimum.commit_sequence
    ));
    assert!(matches!(
        peer_client.query::<ReadCounter>(&fixture.target, reader_node, expected, Some(minimum), 0).await,
        Err(crab_cell_runtime::Error::ReplicaBehind { observed_sequence, minimum_sequence })
            if observed_sequence < minimum_sequence && minimum_sequence == minimum.commit_sequence
    ));
    assert_eq!(
        configured
            .query::<ReadCounter>(&fixture.target, None, 0)
            .await
            .unwrap()
            .output,
        1
    );
    assert_eq!(
        replica_client
            .query::<ReadCounter>(&fixture.target, None, 0)
            .await
            .unwrap()
            .output,
        0
    );
    assert!(matches!(
        replica_client
            .query::<ReadCounter>(&fixture.target, Some(minimum), 0)
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::ReplicaBehind { .. }
        ))
    ));
    let withdrawn_policy = policy.update(&selected_policy, 0).await.unwrap();
    assert!(matches!(
        replica_client
            .query::<ReadCounter>(&fixture.target, None, 0)
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::ReplicaUnavailable
        ))
    ));
    policy.update(&withdrawn_policy, 1).await.unwrap();
    // Owner-ordered streams keep their watermark contract even on a capability
    // whose ordinary queries explicitly select lagging snapshots.
    let mut stream = replica_client
        .open_state_stream::<ReadCounter>(
            &fixture.target,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(stream.emit(0).await.unwrap().output, 1);
    drop(stream);
    drop(peer_client);

    let refreshed_path = fixture._directory.path().join("refreshed.sqlite");
    std::fs::write(&refreshed_path, b"occupied").unwrap();
    assert!(reader.refresh(&refreshed_path).await.is_err());
    assert_eq!(
        reader.query::<ReadCounter>(None, 0).await.unwrap().output,
        0
    );
    std::fs::remove_file(&refreshed_path).unwrap();
    let pending_reader = reader.clone();
    let pending = tokio::spawn(async move { pending_reader.query::<ReadCounter>(None, 99).await });
    tokio::task::spawn_blocking(|| query_barriers().0.wait())
        .await
        .unwrap();
    let refreshed = reader.clone();
    assert_eq!(
        refreshed
            .refresh(&refreshed_path)
            .await
            .unwrap()
            .commit_sequence,
        committed.commit_sequence()
    );
    assert!(reader_path.exists());
    let during_refresh = reader_runtime.stats();
    tokio::task::spawn_blocking(|| query_barriers().1.wait())
        .await
        .unwrap();
    let old = pending.await.unwrap().unwrap();
    assert_eq!(during_refresh.worker_jobs(), 1);
    assert_eq!(reader_runtime.stats().worker_jobs(), 0);
    assert_eq!(during_refresh.file_descriptors(), 8);
    assert_eq!(old.output, 0);
    assert!(old.receipt.commit_sequence < refreshed.receipt().await.commit_sequence);
    assert!(!reader_path.exists());
    assert_eq!(reader_runtime.stats().file_descriptors(), 4);
    assert_eq!(during_refresh.local_disk_reserved_bytes(), disk_bytes);
    assert_eq!(during_refresh.resident_bytes(), 24 << 20);
    assert_eq!(
        reader.query::<ReadCounter>(None, 0).await.unwrap().output,
        1
    );
    assert_eq!(
        refreshed
            .query::<ReadCounter>(None, 0)
            .await
            .unwrap()
            .output,
        1
    );

    lifecycle::drain_waits_for_replica_sql(fixture, &registry, &directory).await;

    let advertised = directory.load(session, now_ms()).await.unwrap().unwrap();
    directory.withdraw(&advertised, now_ms()).await.unwrap();
    let (warm, ready) = reader.readiness().await.unwrap();
    assert!(!ready);
    assert_eq!(warm.commit_sequence, committed.commit_sequence());
    assert!(matches!(
        reader.query::<ReadCounter>(None, 0).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));

    handle.drain().await.unwrap();
    assert!(runtime.active_catalog_entries().await.unwrap().is_empty());
    assert!(matches!(
        reader.query::<ReadCounter>(None, 0).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    assert!(matches!(
        refreshed.query::<ReadCounter>(None, 0).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    assert!(matches!(
        reader.readiness().await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    reader.close();
    assert!(matches!(
        refreshed.readiness().await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    assert!(matches!(
        replica_client
            .query::<ReadCounter>(&fixture.target, None, 0)
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::Fenced
        ))
    ));
    drop(replica_client);
    drop(local_client);
    drop(configured);
    drop(reader);
    drop(refreshed);
    assert!(!reader_path.exists());
    assert!(!refreshed_path.exists());
    assert_eq!(reader_runtime.stats().file_descriptors(), 0);
    assert_eq!(reader_runtime.stats().resident_bytes(), 0);
    assert_eq!(reader_runtime.stats().local_disk_reserved_bytes(), 0);
    reader_runtime.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}
