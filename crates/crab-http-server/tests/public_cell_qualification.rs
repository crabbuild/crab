use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_app::ApplicationHandle;
use crab_cell_host::CellNodeBuilder;
use crab_cell_runtime::{
    ActivitySupervisor, ApplicationId, CatalogRole, CellClient, CellStorageLayout, CellTarget,
    EffectClaimRequest, Error, MutationIdentity, NodeLeaseGuard, QualificationExecution,
    QualificationOperation, QualificationOperationExecutor, QualificationProfile,
    QualificationWorkload, RequestId, Result, SqlBatch, SqlStatement, SqlValue, SqlWorkerPool,
    TenantId, install_blob_schema, install_cron_schema, install_kv_schema, install_queue_schema,
    install_workflow_schema, partition_for_shard,
};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};
use tokio_util::sync::CancellationToken;

#[path = "support/reference_application.rs"]
mod fixture;

fn identity(index: u64, now_ms: i64) -> MutationIdentity {
    let mut request_id = [0; 16];
    request_id[..8].copy_from_slice(&index.to_be_bytes());
    MutationIdentity {
        request_id: RequestId::from_bytes(request_id),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

struct PublicHostQualificationExecutor {
    handle: ApplicationHandle<fixture::ReferenceApplication>,
    tenant: TenantId,
    application: ApplicationId,
    now_ms: i64,
}

impl QualificationOperationExecutor for PublicHostQualificationExecutor {
    type Future<'a> = Pin<Box<dyn Future<Output = Result<QualificationExecution>> + Send + 'a>>;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        let handle = self.handle.clone();
        let tenant = self.tenant;
        let application = self.application;
        let now_ms = self.now_ms;
        Box::pin(async move {
            let mutation = identity(operation.index(), now_ms);
            match operation.primitive() {
                "sql" => {
                    let target = CellTarget::new(
                        tenant,
                        application,
                        fixture::SQL_NAMESPACE,
                        &partition_for_shard(0),
                    )?;
                    handle
                        .sql::<fixture::ReferenceSql>(target)?
                        .query(
                            None,
                            SqlBatch {
                                statements: vec![SqlStatement {
                                    sql: "SELECT ?1".into(),
                                    parameters: vec![SqlValue::Integer(operation.nonce() as i64)],
                                }],
                            },
                        )
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification SQL invocation failed")
                        })?;
                }
                "kv" => {
                    handle
                        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)?
                        .atomic(
                            mutation,
                            crab_cell_runtime::KvAtomicRequest {
                                scope: b"public-qualification".to_vec(),
                                checks: Vec::new(),
                                mutations: vec![crab_cell_runtime::KvMutation::Put {
                                    key: operation.index().to_be_bytes().to_vec(),
                                    value: operation.nonce().to_be_bytes().to_vec(),
                                    expires_at_ms: None,
                                }],
                            },
                        )
                        .await
                        .map_err(|_| Error::Control("public qualification KV invocation failed"))?;
                }
                "blob" => {
                    handle
                        .blob::<fixture::ReferenceBlob>()?
                        .list_shard(0, Vec::new(), None, 1, None)
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Blob invocation failed")
                        })?;
                }
                "queue" => {
                    handle
                        .queue::<fixture::ReferenceQueue>()?
                        .info(0, None)
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Queue invocation failed")
                        })?;
                }
                "cron" => {
                    handle
                        .cron::<fixture::ReferenceCron>()?
                        .get([58; 16], None)
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Cron invocation failed")
                        })?;
                }
                "workflow" => {
                    handle
                        .workflow::<fixture::ReferenceWorkflow>()?
                        .state(b"public-qualification".to_vec(), None)
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Workflow invocation failed")
                        })?;
                }
                "activity" => {
                    let supervisor = ActivitySupervisor::new(
                        handle.activities::<fixture::ReferenceWorkflow>()?,
                        5_000,
                    )?;
                    supervisor.run_once(0, None).await.map_err(|_| {
                        Error::Control("public qualification Activity invocation failed")
                    })?;
                }
                "effects" => {
                    let target = CellTarget::new(
                        tenant,
                        application,
                        fixture::WORKFLOW_NAMESPACE,
                        &partition_for_shard(0),
                    )?;
                    handle
                        .effects::<fixture::ReferenceWorkflow>(target)?
                        .claim(
                            mutation,
                            EffectClaimRequest {
                                limit: 1,
                                lease_ms: 5_000,
                            },
                        )
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Effects invocation failed")
                        })?;
                }
                _ => {
                    return Err(Error::Control(
                        "public qualification primitive is not registered",
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok(QualificationExecution::acknowledged(true))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_cell_node_runs_typed_primitive_workload() {
    let application = Arc::new(fixture::compiled());
    let tenant = TenantId::from_bytes([71; 16]);
    let application_id = ApplicationId::from_bytes([72; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("public-host-qualification"),
        *application_id.as_bytes(),
    );
    let directory = tempfile::tempdir().expect("qualification directory");
    let session = crab_cell_runtime::SessionId::from_bytes([24; 16]);
    let node = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(
            SqlWorkerPool::new(4, 32).expect("qualification pool"),
            64 * 1024 * 1024,
        )
        .with_replica_host(fixture::reference_host())
        .with_session(session)
        .build()
        .expect("qualification node");
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .expect("qualification task group");
    node.install_node_lease(NodeLeaseGuard::new(0, 60_000).expect("qualification lease"))
        .expect("qualification readiness");
    assert!(node.is_ready());

    let runtime = node.runtime();
    let registry = application.registry();
    let handles = vec![
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::SQL_NAMESPACE,
            CatalogRole::Sql,
            fixture::SQL_MODULE,
            40,
            |_| Ok(()),
        )
        .await
        .expect("SQL Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::KV_NAMESPACE,
            CatalogRole::Kv,
            fixture::KV_MODULE,
            41,
            install_kv_schema,
        )
        .await
        .expect("KV Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::BLOB_NAMESPACE,
            CatalogRole::Blob,
            fixture::BLOB_MODULE,
            42,
            install_blob_schema,
        )
        .await
        .expect("Blob Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::QUEUE_NAMESPACE,
            CatalogRole::Queue,
            fixture::QUEUE_MODULE,
            43,
            install_queue_schema,
        )
        .await
        .expect("Queue Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::DEAD_LETTER_NAMESPACE,
            CatalogRole::Queue,
            fixture::DEAD_LETTER_MODULE,
            44,
            install_queue_schema,
        )
        .await
        .expect("dead-letter Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::CRON_NAMESPACE,
            CatalogRole::Cron,
            fixture::CRON_MODULE,
            45,
            install_cron_schema,
        )
        .await
        .expect("Cron Cell"),
        fixture::bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            fixture::WORKFLOW_NAMESPACE,
            CatalogRole::Workflow,
            fixture::WORKFLOW_MODULE,
            46,
            install_workflow_schema,
        )
        .await
        .expect("Workflow Cell"),
    ];
    let client = CellClient::local_many(registry, handles).expect("qualification client");
    let typed =
        node.application_handle::<fixture::ReferenceApplication>(client, tenant, application_id);
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_millis(),
    )
    .expect("current epoch fits mutation identity");
    let profile = QualificationProfile::new("public-host-primitives".into(), 1, 16, 1, 5_000)
        .expect("qualification profile");
    let workload = QualificationWorkload::generate_with_size(&profile, 91, 1, 16, 1)
        .expect("qualification workload");
    let mut executor = PublicHostQualificationExecutor {
        handle: typed.clone(),
        tenant,
        application: application_id,
        now_ms,
    };
    let summary = workload.run(&mut executor).await.expect("typed workload");
    assert_eq!(summary.operations(), 16);
    assert!(summary.primitive_counts().iter().all(|counts| {
        counts.attempted() > 0 && counts.acknowledged() > 0 && counts.verified() > 0
    }));
    let artifact = summary.artifact(&workload).expect("run artifact");
    artifact
        .verify_for_profile(&profile)
        .expect("run artifact profile");
    assert!(!artifact.encode().expect("run artifact encoding").is_empty());

    drop(typed);
    node.shutdown().await.expect("qualification shutdown");
    assert!(!node.is_ready());
}
