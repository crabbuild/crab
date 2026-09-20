use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_app::ApplicationHandle;
use crab_cell_host::CellNodeBuilder;
use crab_cell_runtime::{
    ActivitySupervisor, ApplicationId, BlobCondition, BlobMutation, BlobQuery, CatalogRole,
    CellClient, CellStorageLayout, CellTarget, CronMutation, Digest, EffectClaimRequest, Error,
    KvAtomicRequest, KvMutation, MutationIdentity, NodeLeaseGuard, QualificationExecution,
    QualificationOperation, QualificationOperationExecutor, QualificationProfile,
    QualificationRunner, QualificationWorkload, QueueClaimRequest, QueueSendRequest, RequestId,
    Result, SqlBatch, SqlStatement, SqlValue, SqlWorkerPool, TenantId, WorkflowOutcome,
    WorkflowSignal, install_blob_schema, install_cron_schema, install_kv_schema,
    install_queue_schema, install_workflow_schema, partition_for_shard,
};
use crab_storage::Store;
use ed25519_dalek::SigningKey;
use object_store::{memory::InMemory, path::Path};
use tokio_util::sync::CancellationToken;

#[path = "support/reference_application.rs"]
mod fixture;

fn identity(index: u64, now_ms: i64) -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes(fixed_id(index)),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

fn fixed_id(index: u64) -> [u8; 16] {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&index.to_be_bytes());
    id
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
            let mutation_index = operation.index().saturating_mul(100);
            let mutation = identity(mutation_index, now_ms);
            if operation.rejection_hint() {
                let wrong_target = CellTarget::new(
                    tenant,
                    application,
                    fixture::KV_NAMESPACE,
                    &partition_for_shard(0),
                )?;
                if handle.sql::<fixture::ReferenceSql>(wrong_target).is_ok() {
                    return Err(Error::Control(
                        "public qualification rejection was accepted",
                    ));
                }
                return Ok(QualificationExecution::rejected());
            }
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
                    let kv = handle.kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)?;
                    let scope = b"public-qualification".to_vec();
                    let key = operation.index().to_be_bytes().to_vec();
                    let request = KvAtomicRequest {
                        scope: scope.clone(),
                        checks: Vec::new(),
                        mutations: vec![KvMutation::Put {
                            key: key.clone(),
                            value: operation.nonce().to_be_bytes().to_vec(),
                            expires_at_ms: None,
                        }],
                    };
                    kv.atomic(mutation, request.clone())
                        .await
                        .map_err(|_| Error::Control("public qualification KV invocation failed"))?;
                    kv.atomic(mutation, request)
                        .await
                        .map_err(|_| Error::Control("public qualification KV duplicate failed"))?;
                    let expires_at_ms = i64::try_from(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| Error::Control("public qualification clock failed"))?
                            .as_millis(),
                    )
                    .map_err(|_| Error::Control("public qualification clock overflow"))?
                    .saturating_add(20);
                    let expired_key = fixed_id(operation.index()).to_vec();
                    kv.atomic(
                        identity(mutation_index.saturating_add(1), now_ms),
                        KvAtomicRequest {
                            scope: scope.clone(),
                            checks: Vec::new(),
                            mutations: vec![KvMutation::Put {
                                key: expired_key.clone(),
                                value: b"expired".to_vec(),
                                expires_at_ms: Some(expires_at_ms),
                            }],
                        },
                    )
                    .await
                    .map_err(|_| Error::Control("public qualification KV expiry failed"))?;
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    let expired = kv.get(scope, expired_key, None).await.map_err(|_| {
                        Error::Control("public qualification KV verification failed")
                    })?;
                    if expired.output.is_some() {
                        return Err(Error::Control("public qualification KV expiry visible"));
                    }
                }
                "blob" => {
                    let blob = handle.blob::<fixture::ReferenceBlob>()?;
                    let key = format!("public-qualification/{}", operation.index()).into_bytes();
                    let upload_id = fixed_id(operation.index());
                    blob.mutate(
                        mutation,
                        BlobMutation::Begin {
                            key: key.clone(),
                            upload_id,
                            condition: BlobCondition::Missing,
                            content_type: None,
                            metadata: Vec::new(),
                            expires_at_ms: now_ms + 60_000,
                        },
                    )
                    .await
                    .map_err(|_| Error::Control("public qualification Blob begin failed"))?;
                    blob.mutate(
                        identity(mutation_index.saturating_add(1), now_ms),
                        BlobMutation::PutPart {
                            key: key.clone(),
                            upload_id,
                            part_number: 1,
                            payload: operation.nonce().to_be_bytes().to_vec(),
                        },
                    )
                    .await
                    .map_err(|_| Error::Control("public qualification Blob part failed"))?;
                    blob.mutate(
                        identity(mutation_index.saturating_add(2), now_ms),
                        BlobMutation::Complete {
                            key: key.clone(),
                            upload_id,
                            part_count: 1,
                        },
                    )
                    .await
                    .map_err(|_| Error::Control("public qualification Blob complete failed"))?;
                    blob.query(
                        BlobQuery::Read {
                            key,
                            offset: 0,
                            limit: 128,
                        },
                        None,
                    )
                    .await
                    .map_err(|_| Error::Control("public qualification Blob verification failed"))?;
                }
                "queue" => {
                    let queue = handle.queue::<fixture::ReferenceQueue>()?;
                    let producer_id = fixed_id(operation.index());
                    queue
                        .send(
                            mutation,
                            QueueSendRequest {
                                producer_id,
                                payload: operation.nonce().to_be_bytes().to_vec(),
                                available_at_ms: now_ms,
                            },
                        )
                        .await
                        .map_err(|_| Error::Control("public qualification Queue send failed"))?;
                    let claimed = queue
                        .claim(
                            identity(mutation_index.saturating_add(1), now_ms),
                            0,
                            QueueClaimRequest {
                                limit: 1,
                                lease_ms: 5_000,
                            },
                        )
                        .await
                        .map_err(|_| Error::Control("public qualification Queue claim failed"))?;
                    let message = claimed
                        .output
                        .first()
                        .cloned()
                        .ok_or(Error::Control("public qualification Queue claim empty"))?;
                    queue
                        .retry(
                            identity(mutation_index.saturating_add(2), now_ms),
                            0,
                            message.message_id,
                            message.token,
                            0,
                        )
                        .await
                        .map_err(|_| Error::Control("public qualification Queue retry failed"))?;
                    let retried = queue
                        .claim(
                            identity(mutation_index.saturating_add(3), now_ms),
                            0,
                            QueueClaimRequest {
                                limit: 1,
                                lease_ms: 5_000,
                            },
                        )
                        .await
                        .map_err(|_| Error::Control("public qualification Queue reclaim failed"))?;
                    let retried_message = retried
                        .output
                        .first()
                        .cloned()
                        .ok_or(Error::Control("public qualification Queue reclaim empty"))?;
                    queue
                        .ack(
                            identity(mutation_index.saturating_add(4), now_ms),
                            0,
                            retried_message.message_id,
                            retried_message.token,
                        )
                        .await
                        .map_err(|_| Error::Control("public qualification Queue ack failed"))?;
                }
                "cron" => {
                    let cron = handle.cron::<fixture::ReferenceCron>()?;
                    let schedule_id = fixed_id(operation.index());
                    cron.mutate(
                        mutation,
                        CronMutation::Upsert {
                            schedule_id,
                            target_index: 0,
                            target_partition: partition_for_shard(0).to_vec(),
                            payload: operation.nonce().to_be_bytes().to_vec(),
                            interval_ms: 1_000,
                            next_due_ms: now_ms + 1_000,
                        },
                    )
                    .await
                    .map_err(|_| Error::Control("public qualification Cron upsert failed"))?;
                    cron.mutate(
                        identity(mutation_index.saturating_add(1), now_ms),
                        CronMutation::Pause { schedule_id },
                    )
                    .await
                    .map_err(|_| Error::Control("public qualification Cron pause failed"))?;
                    cron.mutate(
                        identity(mutation_index.saturating_add(2), now_ms),
                        CronMutation::Resume {
                            schedule_id,
                            next_due_ms: now_ms + 1_000,
                        },
                    )
                    .await
                    .map_err(|_| Error::Control("public qualification Cron resume failed"))?;
                    cron.get(schedule_id, None).await.map_err(|_| {
                        Error::Control("public qualification Cron verification failed")
                    })?;
                }
                "workflow" => {
                    let workflow = handle.workflow::<fixture::ReferenceWorkflow>()?;
                    let workflow_id = format!("public-workflow-{}", operation.index()).into_bytes();
                    let started = workflow
                        .start(mutation, workflow_id.clone(), b"activity".to_vec())
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Workflow start failed")
                        })?;
                    let WorkflowOutcome::Applied { run_id, .. } = started.output else {
                        return Err(Error::Control("public qualification Workflow not applied"));
                    };
                    workflow
                        .cancel(
                            identity(mutation_index.saturating_add(1), now_ms),
                            WorkflowSignal {
                                workflow_id: workflow_id.clone(),
                                run_id,
                                signal_id: fixed_id(operation.index()),
                                event: b"cancel".to_vec(),
                            },
                        )
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Workflow cancel failed")
                        })?;
                    workflow.state(workflow_id, None).await.map_err(|_| {
                        Error::Control("public qualification Workflow verification failed")
                    })?;
                }
                "activity" => {
                    let workflow = handle.workflow::<fixture::ReferenceWorkflow>()?;
                    let workflow_id = format!("public-activity-{}", operation.index()).into_bytes();
                    workflow
                        .start(mutation, workflow_id.clone(), b"activity".to_vec())
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Activity start failed")
                        })?;
                    let supervisor = ActivitySupervisor::new(
                        handle.activities::<fixture::ReferenceWorkflow>()?,
                        5_000,
                    )?;
                    supervisor.run_once(0, None).await.map_err(|_| {
                        Error::Control("public qualification Activity invocation failed")
                    })?;
                    workflow.state(workflow_id, None).await.map_err(|_| {
                        Error::Control("public qualification Activity verification failed")
                    })?;
                }
                "effects" => {
                    let workflow = handle.workflow::<fixture::ReferenceWorkflow>()?;
                    let workflow_id = format!("public-effect-{}", operation.index()).into_bytes();
                    workflow
                        .start(mutation, workflow_id, b"effect".to_vec())
                        .await
                        .map_err(|_| Error::Control("public qualification Effect start failed"))?;
                    let target = CellTarget::new(
                        tenant,
                        application,
                        fixture::WORKFLOW_NAMESPACE,
                        &partition_for_shard(0),
                    )?;
                    let effects = handle.effects::<fixture::ReferenceWorkflow>(target)?;
                    let claims = effects
                        .claim(
                            identity(mutation_index.saturating_add(1), now_ms),
                            EffectClaimRequest {
                                limit: 1,
                                lease_ms: 5_000,
                            },
                        )
                        .await
                        .map_err(|_| Error::Control("public qualification Effects claim failed"))?;
                    let claim = claims
                        .output
                        .first()
                        .cloned()
                        .ok_or(Error::Control("public qualification Effects claim empty"))?;
                    effects
                        .ack(
                            identity(mutation_index.saturating_add(2), now_ms),
                            claim,
                            b"public-effect-result".to_vec(),
                        )
                        .await
                        .map_err(|_| Error::Control("public qualification Effects ack failed"))?;
                }
                _ => {
                    return Err(Error::Control(
                        "public qualification primitive is not registered",
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
            if operation.ambiguous_hint() {
                Ok(QualificationExecution::ambiguous(1))
            } else {
                Ok(QualificationExecution::acknowledged(true)
                    .with_retries(u64::from(operation.retry_hint())))
            }
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
    let profile = QualificationProfile::pr_contract();
    let workload = QualificationWorkload::generate_with_size(&profile, 41, 1, 64, 1)
        .expect("qualification workload");
    let mut executor = PublicHostQualificationExecutor {
        handle: typed.clone(),
        tenant,
        application: application_id,
        now_ms,
    };
    let summary = workload.run(&mut executor).await.expect("typed workload");
    assert_eq!(summary.operations(), 64);
    assert!(summary.primitive_counts().iter().all(|counts| {
        counts.attempted() > 0 && counts.acknowledged() > 0 && counts.verified() > 0
    }));
    assert!(
        summary
            .primitive_counts()
            .iter()
            .any(|counts| counts.retried() > 0)
    );
    assert!(
        summary
            .primitive_counts()
            .iter()
            .any(|counts| counts.rejected() > 0)
    );
    let artifact = summary.artifact(&workload).expect("run artifact");
    artifact
        .verify_for_profile(&profile)
        .expect("run artifact profile");
    let artifact_bytes = artifact.encode().expect("run artifact encoding");
    let signing_key = SigningKey::from_bytes(&[75; 32]);
    let trusted_signer = signing_key.verifying_key().to_bytes();
    let image = Digest::from_bytes([73; 32]);
    let receipt = QualificationRunner::new(signing_key)
        .emit_with_profile_and_evidence(
            &profile,
            "public-host-source".into(),
            image,
            "local".into(),
            "primitives".into(),
            "none".into(),
            summary.metrics().expect("receipt metrics"),
            &artifact_bytes,
            true,
            (
                "rustc".into(),
                "debug".into(),
                "local".into(),
                workload.seed(),
                0,
                0,
                false,
            ),
            1,
            2,
            b"public-host",
            vec![Digest::from_bytes(
                *blake3::hash(&artifact_bytes).as_bytes(),
            )],
            Vec::new(),
        )
        .expect("qualification receipt");
    receipt
        .verify_for_profile_with_signer(
            "public-host-source",
            image,
            &profile,
            &[&artifact_bytes],
            trusted_signer,
        )
        .expect("qualification receipt verification");

    drop(typed);
    node.shutdown().await.expect("qualification shutdown");
    assert!(!node.is_ready());
}
