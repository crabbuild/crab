//! Additive application code, retained clients, and exact-root recovery.

use super::*;
use crate::reference_application::performance_fixture::rustfs_store;
use crab_cell_host::CellNodeBuilder;
use crab_cell_runtime::node::lease::NodeLeaseGuard;
use crab_cell_runtime::registry::{Query, QueryContext, RetainedCodeDescriptor};
use tokio_util::sync::CancellationToken;

struct SuccessorSql;

impl CellModule for SuccessorSql {
    const NAME: &'static str = SQL_MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| {
            let predecessor = ReferenceSql.descriptor();
            let mut queries = predecessor.queries.to_vec();
            queries.push(operation(ReceiptPayload::ID));
            ModuleDescriptor {
                source_digest: Digest::from_bytes(*blake3::hash(b"reference-sql-v2").as_bytes()),
                retained_codes: Box::leak(Box::new([RetainedCodeDescriptor {
                    code: compiled().registry().module_code(SQL_MODULE).unwrap(),
                    schema_min: 1,
                    schema_max: 1,
                }])),
                queries: Box::leak(queries.into_boxed_slice()),
                ..*predecessor
            }
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        ReferenceSql.register(registry)?;
        registry.bind_query::<ReceiptPayload>()
    }
}

struct ReceiptPayload;

impl Query for ReceiptPayload {
    const MODULE: &'static str = SQL_MODULE;
    const ID: u32 = 8;
    const CODEC_VERSION: u32 = 1;
    type Input = u64;
    type Output = Vec<u8>;

    fn execute(context: &mut QueryContext<'_>, occurrence: u64) -> Result<Vec<u8>> {
        let occurrence = i64::try_from(occurrence)
            .map_err(|_| Error::Command("receipt occurrence exceeds SQL integer range"))?;
        let results = context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "SELECT payload FROM invoice_receipts WHERE occurrence = ?1".into(),
                parameters: vec![SqlValue::Integer(occurrence)],
            }],
        })?;
        match results[0].rows.first().and_then(|row| row.first()) {
            Some(SqlValue::Blob(payload)) => Ok(payload.clone()),
            _ => Err(Error::Command("receipt payload is unavailable")),
        }
    }
}

struct SuccessorApplication;

impl CellApplication for SuccessorApplication {
    const NAME: &'static str = ReferenceApplication::NAME;

    fn register(builder: &mut ApplicationBuilder) -> Result<()> {
        ReferenceApplication::register_with_sql(builder, SuccessorSql)
    }
}

crab_cell_app::cell_client! {
    struct SuccessorClient (SuccessorApplication) {
        fn orders(scope: &OrderId) -> SuccessorOrder {
            namespace: SQL_NAMESPACE,
            module: SQL_MODULE,
            commands: { fn receive_cron, prepare_receive_cron: ReferenceCronReceiver = 6; },
            queries: {
                fn receipt_count: ReferenceReceiptCount = 7;
                fn receipt_payload: ReceiptPayload = 8;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_host_additive_code_rollout_preserves_acknowledged_state() {
    verify_rollout(
        Store::new(Arc::new(InMemory::new())),
        Path::from("additive-rollout"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual additive release qualification with real RustFS"]
async fn three_node_host_rustfs_additive_code_rollout() {
    let root = std::env::var("CRAB_CELL_PERF_PROCESS_ROOT").unwrap();
    verify_rollout(rustfs_store(), Path::from(root)).await;
}

async fn verify_rollout(store: Store, root: Path) {
    let predecessor = compiled();
    let successor = Arc::new(
        SuccessorApplication::compile(BuildDescriptor {
            source_revision: "reference-additive-successor".into(),
            cargo_lock_digest: Digest::from_bytes([42; 32]),
        })
        .unwrap(),
    );
    let registry = successor.registry();
    let old_code = predecessor.registry().module_code(SQL_MODULE).unwrap();
    let new_code = registry.module_code(SQL_MODULE).unwrap();
    assert_ne!(old_code, new_code);
    registry
        .verify_rolling_from(predecessor.registry().release_bytes())
        .unwrap();
    assert!(
        predecessor
            .registry()
            .verify_rolling_from(registry.release_bytes())
            .is_err()
    );

    let fixture = PerfFixture::start_configured(3, Some(Arc::clone(&successor)), store, root).await;
    let target = &fixture.sql_target;
    let owner = fixture.owned_handles[0]
        .iter()
        .find(|handle| handle.cell_id() == target.cell_id())
        .unwrap()
        .clone();
    assert_eq!(owner.code(), old_code);
    let local = fixture.nodes[0]
        .application_handle::<SuccessorApplication>(
            CellClient::local(Arc::clone(&registry), owner.clone()),
            target.tenant(),
            target.application(),
        )
        .unwrap();
    let local = SuccessorClient::new(local).unwrap();

    // The old release enters the successor's actual dispatcher over TCP. Its
    // retained code is executable until the operator publishes the new code.
    let old = ReferenceClient::new(fixture.typed.clone()).unwrap();
    let key = OrderId(b"additive-rollout-order".to_vec());
    let old_order = old.orders(&key).unwrap();
    let local_order = local.orders(&key).unwrap();
    let original_identity = reference_identity(76, now_ms());
    let original_input = invocation(1);
    let (original, added) = tokio::join!(
        old_order.receive_cron(original_identity, original_input.clone()),
        local_order.receive_cron(reference_identity(77, now_ms()), invocation(2)),
    );
    let original = original.unwrap();
    added.unwrap();
    assert_eq!(old_order.receipt_count(None, ()).await.unwrap().output, 2);
    assert_eq!(
        local_order.receipt_payload(None, 1).await.unwrap().output,
        original_input.payload
    );
    let prepared_before_cutover = local_order
        .prepare_receive_cron(reference_identity(78, now_ms()), invocation(3))
        .await
        .unwrap();

    let plan = registry
        .next_migration(SQL_NAMESPACE, owner.code(), owner.schema())
        .unwrap()
        .unwrap();
    let migrated = owner.migrate(plan, now_ms()).await.unwrap();
    assert_eq!(migrated.handle.code(), new_code);
    assert!(matches!(
        prepared_before_cutover.execute().await,
        Err(InvocationError::NotStarted(Error::CellDraining))
    ));
    let layout = fixture.layout.as_ref().unwrap();
    let authority = CellAuthority::new(layout.clone());
    let published = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(published.value().code, new_code);
    assert_eq!(
        published.value().root.as_ref().unwrap().commit_sequence,
        migrated.outcome.commit_sequence
    );
    let retired_client = fixture.nodes[1]
        .application_handle::<ReferenceApplication>(
            CellClient::local(predecessor.registry(), migrated.handle.clone()),
            target.tenant(),
            target.application(),
        )
        .unwrap();
    let retired_client = ReferenceClient::new(retired_client).unwrap();
    assert!(matches!(
        retired_client
            .orders(&key)
            .unwrap()
            .receipt_count(None, ())
            .await,
        Err(InvocationError::NotStarted(Error::Command(
            "Cell code does not match operation module"
        )))
    ));

    // Rebind the peer listener to the published capability and independently
    // authenticate the upgraded client's release. No stale handle is reused.
    let signer = Arc::new(PeerSigner::new(
        node_session(2),
        registry.release_digest(),
        SigningKey::from_bytes(&[79; 32]),
    ));
    let verifier = Arc::new(PeerVerifier::new(
        node_session(2),
        registry.release_digest(),
        signer.verifying_key(),
    ));
    let (address, server) =
        start_peer_server(&registry, verifier, vec![migrated.handle.clone()], None).await;
    let peer = CellClient::peer(
        Arc::clone(&registry),
        signer,
        PeerPrincipal {
            issuer: "reference-rollout".into(),
            subject: "upgraded-client".into(),
            actions: vec!["cell.read".into(), "cell.write".into()],
        },
        peer_round_trip(HashMap::from([(target.cell_id(), address)])),
    );
    let upgraded = fixture.nodes[0]
        .application_handle::<SuccessorApplication>(peer, target.tenant(), target.application())
        .unwrap();
    let upgraded = SuccessorClient::new(upgraded).unwrap();
    let order = upgraded.orders(&key).unwrap();
    let replay = order
        .receive_cron(original_identity, original_input.clone())
        .await
        .unwrap();
    assert_eq!(replay.receipt, original.receipt);
    order
        .receive_cron(reference_identity(79, now_ms()), invocation(3))
        .await
        .unwrap();
    assert_eq!(order.receipt_count(None, ()).await.unwrap().output, 3);
    assert_eq!(
        order
            .receipt_payload(Some(replay.receipt), 1)
            .await
            .unwrap()
            .output,
        original_input.payload
    );

    // Retire an old host, then recover on a fresh successor host with no shared
    // SQLite directory. The object root must carry both code and dedup state.
    fixture.nodes[1].shutdown().await.unwrap();
    server.abort();
    migrated.handle.drain().await.unwrap();
    let replacement = CellNodeBuilder::new(successor)
        .with_runtime(SqlWorkerPool::new(2, 8).unwrap(), 64 << 20)
        .with_session(node_session(3))
        .with_replica_host(reference_host())
        .build()
        .unwrap();
    replacement
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    replacement
        .install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    let files = tempfile::TempDir::new().unwrap();
    let observed = authority.load(target.cell_id()).await.unwrap().unwrap();
    let restored = replacement
        .runtime()
        .acquire_idle_restored(
            CellCatalog::new(layout.clone(), target.tenant())
                .lookup(target.cell_id())
                .await
                .unwrap()
                .unwrap(),
            CellReplica::new(
                layout.clone(),
                *target.cell_id().as_bytes(),
                *IncarnationId::from_bytes([40; 16]).as_bytes(),
                reference_limits(SQL_NAMESPACE).unwrap(),
            )
            .unwrap(),
            authority,
            observed,
            files.path().join("restored.sqlite"),
            Owner {
                session: node_session(3),
                endpoint: "https://reference-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(restored.code(), new_code);
    let recovered = replacement
        .application_handle::<SuccessorApplication>(
            CellClient::local(registry, restored),
            target.tenant(),
            target.application(),
        )
        .unwrap();
    let recovered = SuccessorClient::new(recovered).unwrap();
    let recovered_order = recovered.orders(&key).unwrap();
    let replay = recovered_order
        .receive_cron(original_identity, original_input.clone())
        .await
        .unwrap();
    assert_eq!(replay.receipt, original.receipt);
    assert_eq!(
        recovered_order
            .receipt_count(None, ())
            .await
            .unwrap()
            .output,
        3
    );
    assert_eq!(
        recovered_order
            .receipt_payload(None, 1)
            .await
            .unwrap()
            .output,
        original_input.payload
    );
    recovered_order
        .receive_cron(reference_identity(80, now_ms()), invocation(4))
        .await
        .unwrap();
    assert_eq!(
        recovered_order
            .receipt_count(None, ())
            .await
            .unwrap()
            .output,
        4
    );
    replacement.shutdown().await.unwrap();
    fixture.shutdown().await;
    println!(
        "ROLLOUT additive_code: old_code={old_code:?} new_code={new_code:?} exact_receipts=4 duplicate_replays=2 recovered=true"
    );
}
