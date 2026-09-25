//! The typed-handle commit path through the reference application.

use super::performance_fixture::PerfFixture;
use crate::*;

mod wrong_generated_ids {
    use super::*;

    crab_cell_app::cell_client! {
        pub(super) struct WrongClient (ReferenceApplication) {
            pub(super) fn orders(scope: &OrderId) -> WrongOrderCell {
                namespace: SQL_NAMESPACE,
                module: SQL_MODULE,
                commands: { pub(super) fn receive, prepare_receive: ReferenceCronReceiver = 99; },
                queries: { }
            }
        }
    }
}

struct WrongApplication;

impl CellApplication for WrongApplication {
    const NAME: &'static str = "wrong-application";

    fn register(_builder: &mut ApplicationBuilder) -> Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn application_handle_rejects_mismatched_author_and_client_registry() {
    let fixture = PerfFixture::start(1).await;
    let tenant = fixture.sql_target.tenant();
    let application_id = fixture.sql_target.application();
    let compiled = Arc::new(compiled());
    assert!(
        ApplicationHandle::<WrongApplication>::new(
            fixture.client.clone(),
            Arc::clone(&compiled),
            tenant,
            application_id,
        )
        .is_err()
    );
    let other_release = ReferenceApplication::compile(BuildDescriptor {
        source_revision: "different-source".into(),
        cargo_lock_digest: Digest::from_bytes([42; 32]),
    })
    .unwrap();
    assert!(
        ApplicationHandle::<ReferenceApplication>::new(
            fixture.client.clone(),
            Arc::new(other_release),
            tenant,
            application_id,
        )
        .is_err()
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_client_rejects_command_id_outside_descriptor() {
    let fixture = PerfFixture::start(1).await;
    assert!(matches!(
        wrong_generated_ids::WrongClient::new(fixture.typed.clone()),
        Err(Error::Registry("generated command differs from stable ID"))
    ));
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_client_derives_target_and_commits_bound_operation() {
    let fixture = PerfFixture::start(1).await;
    let client = ReferenceClient::new(fixture.typed.clone()).unwrap();
    let order = client.orders(&OrderId(b"order-42".to_vec())).unwrap();
    assert_eq!(order.target(), &fixture.sql_target);
    let committed = order
        .receive_cron(
            reference_identity(31, super::performance_fixture::now_ms()),
            CronInvocation {
                schedule_id: [32; 16],
                generation: 1,
                occurrence: 1,
                scheduled_at_ms: super::performance_fixture::now_ms(),
                payload: b"generated-client".to_vec(),
            },
        )
        .await
        .unwrap();
    let observed = order
        .receipt_count(Some(committed.receipt), ())
        .await
        .unwrap();
    assert_eq!(observed.output, 1);
    fixture.shutdown().await;
}

#[allow(dead_code)]
fn typed_capability_surface<A: CellApplication>(
    handle: &ApplicationHandle<A>,
    target: CellTarget,
) -> Result<()> {
    let _sql = handle.sql::<ReferenceSql>(target.clone())?;
    let _kv = handle.kv::<ReferenceKv>(KV_NAMESPACE)?;
    let _blob = handle.blob::<ReferenceBlob>()?;
    let _queue = handle.queue::<ReferenceQueue>()?;
    let _cron = handle.cron::<ReferenceCron>()?;
    let _workflow = handle.workflow::<ReferenceWorkflow>()?;
    let _activities = handle.activities::<ReferenceWorkflow>()?;
    let _effects = handle.effects::<ReferenceSql>(target)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reference_application_uses_typed_handle_for_a_real_commit() {
    let application = Arc::new(compiled());
    let tenant = TenantId::from_bytes([21; 16]);
    let application_id = crab_cell_runtime::ApplicationId::from_bytes([22; 16]);
    let target = CellTarget::new(
        tenant,
        application_id,
        SQL_NAMESPACE,
        &crab_cell_runtime::partition_for_shard(0),
    )
    .unwrap();
    let incarnation = IncarnationId::from_bytes([23; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(
        store.clone(),
        object_store::path::Path::from("reference-application"),
        *application_id.as_bytes(),
    );
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(layout.clone(), tenant);
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Sql,
                application.registry().module_code(SQL_MODULE).unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let session = crab_cell_runtime::SessionId::from_bytes([24; 16]);
    let observed = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: "https://reference.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(1, 4).unwrap(),
        16 * 1024 * 1024,
        session,
        reference_host(),
    )
    .unwrap();
    let handle = runtime
        .bootstrap(
            proof,
            CellReplica::new(
                layout,
                *target.cell_id().as_bytes(),
                *incarnation.as_bytes(),
                Limits::default(),
            )
            .unwrap(),
            authority,
            observed,
            directory.path().join("reference.sqlite"),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let client = CellClient::local(application.registry(), handle);
    let typed =
        ApplicationHandle::<ReferenceApplication>::new(client, application, tenant, application_id)
            .unwrap()
            .with_blob_artifact_store(BlobArtifactStore::new(store));
    typed_capability_surface(&typed, target.clone()).unwrap();
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let sql = typed.sql::<ReferenceSql>(target).unwrap();
    let committed = sql
        .batch(
            MutationIdentity {
                request_id: RequestId::from_bytes([25; 16]),
                issued_at_ms: now_ms,
                expires_at_ms: now_ms + 60_000,
            },
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT ?1".into(),
                    parameters: vec![SqlValue::Integer(7)],
                }],
            },
        )
        .await
        .unwrap();
    assert_eq!(committed.output[0].rows.len(), 1);
    runtime.shutdown().await.unwrap();
}
