use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_app::ApplicationHandle;
use crab_cell_host::{CellNode, CellNodeBuilder};
use crab_cell_runtime::{
    ActivityRunOutcome, ActivitySupervisor, ApplicationId, BlobArtifactStore, BlobCondition,
    BlobMutation, BlobQuery, BlobQueryResult, CatalogRole, CellClient, CellStorageLayout,
    CellTarget, CronMutation, CronQueryResult, Digest, EffectClaimRequest, EffectLeaseOutcome,
    Error, KvAtomicOutcome, KvAtomicRequest, KvMutation, NodeLeaseGuard, QUALIFICATION_MATRIX_ROWS,
    QualificationCase, QualificationExecution, QualificationMatrixEntry,
    QualificationMatrixManifest, QualificationOperation, QualificationOperationExecutor,
    QualificationProfile, QualificationReceipt, QualificationRunner, QualificationWorkload,
    QueueClaimRequest, QueueLeaseOutcome, QueueSendOutcome, QueueSendRequest, QueueState, Result,
    SqlBatch, SqlStatement, SqlValue, SqlWorkerPool, TenantId, WorkflowOutcome, WorkflowSignal,
    WorkflowStatus, install_blob_schema, install_cron_schema, install_kv_schema,
    install_queue_schema, install_workflow_schema, partition_for_shard,
};
use crab_storage::Store;
use ed25519_dalek::SigningKey;
use object_store::{memory::InMemory, path::Path};
use tokio_util::sync::CancellationToken;

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;

use qualification::{assert_zero_reservations, fixed_id, identity, rustfs_public_store};

struct PublicHostSmokeExecutor {
    handle: ApplicationHandle<fixture::ReferenceApplication>,
    tenant: TenantId,
    application: ApplicationId,
    run_tag: u64,
}

fn operation_id(run_tag: u64, index: u64) -> u64 {
    run_tag.saturating_mul(1_000_000).saturating_add(index)
}

impl QualificationOperationExecutor for PublicHostSmokeExecutor {
    type Future<'a> = Pin<Box<dyn Future<Output = Result<QualificationExecution>> + Send + 'a>>;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        let handle = self.handle.clone();
        let tenant = self.tenant;
        let application = self.application;
        let run_tag = self.run_tag;
        Box::pin(async move {
            let now_ms = i64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| Error::Control("public qualification clock failed"))?
                    .as_millis(),
            )
            .map_err(|_| Error::Control("public qualification clock overflow"))?;
            let operation_id = operation_id(run_tag, operation.index());
            let sql_row_id = i64::try_from(operation_id)
                .map_err(|_| Error::Control("public qualification row ID overflow"))?;
            let mutation_index = operation_id.saturating_mul(100);
            let mutation = identity(mutation_index, now_ms);
            match operation.primitive() {
                "sql" => {
                    let target = CellTarget::new(
                        tenant,
                        application,
                        fixture::SQL_NAMESPACE,
                        &partition_for_shard(0),
                    )?;
                    let sql = handle.sql::<fixture::ReferenceSql>(target)?;
                    let insert = SqlBatch {
                        statements: vec![SqlStatement {
                            sql: "INSERT INTO qualification_rows (id, payload) VALUES (?1, ?2)"
                                .into(),
                            parameters: vec![
                                SqlValue::Integer(sql_row_id),
                                SqlValue::Blob(operation.nonce().to_be_bytes().to_vec()),
                            ],
                        }],
                    };
                    let inserted = sql
                        .batch(mutation, insert.clone())
                        .await
                        .map_err(|_| Error::Control("public qualification SQL mutation failed"))?;
                    if inserted.output.len() != 1 || inserted.output[0].rows_affected != 1 {
                        return Err(Error::Control("public qualification SQL insert differs"));
                    }
                    if operation.case() == QualificationCase::Duplicate {
                        let duplicate = sql.batch(mutation, insert).await.map_err(|source| {
                            Error::Facility {
                                name: "public qualification SQL duplicate",
                                source: Box::new(source),
                            }
                        })?;
                        if duplicate.receipt != inserted.receipt
                            || duplicate.output != inserted.output
                        {
                            return Err(Error::Control("public qualification SQL replay differs"));
                        }
                    }
                    let queried = sql
                        .query(
                            Some(inserted.receipt),
                            SqlBatch {
                                statements: vec![SqlStatement {
                                    sql: "SELECT payload FROM qualification_rows WHERE id = ?1"
                                        .into(),
                                    parameters: vec![SqlValue::Integer(sql_row_id)],
                                }],
                            },
                        )
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification SQL invocation failed")
                        })?;
                    if queried.output.len() != 1
                        || queried.output[0].rows
                            != vec![vec![SqlValue::Blob(
                                operation.nonce().to_be_bytes().to_vec(),
                            )]]
                    {
                        return Err(Error::Control("public qualification SQL result differs"));
                    }
                }
                "kv" => {
                    let kv = handle.kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)?;
                    let scope = b"public-qualification".to_vec();
                    let key = operation_id.to_be_bytes().to_vec();
                    let request = KvAtomicRequest {
                        scope: scope.clone(),
                        checks: Vec::new(),
                        mutations: vec![KvMutation::Put {
                            key: key.clone(),
                            value: operation.nonce().to_be_bytes().to_vec(),
                            expires_at_ms: None,
                        }],
                    };
                    let written = kv
                        .atomic(mutation, request.clone())
                        .await
                        .map_err(|_| Error::Control("public qualification KV invocation failed"))?;
                    let duplicate = kv
                        .atomic(mutation, request)
                        .await
                        .map_err(|_| Error::Control("public qualification KV duplicate failed"))?;
                    if !matches!(written.output, KvAtomicOutcome::Applied(_))
                        || duplicate.output != written.output
                        || duplicate.receipt != written.receipt
                    {
                        return Err(Error::Control("public qualification KV write differs"));
                    }
                    let observed = kv
                        .get(scope.clone(), key, Some(written.receipt))
                        .await
                        .map_err(|_| Error::Control("public qualification KV read failed"))?;
                    if observed.output.as_ref().map(|entry| entry.value.as_slice())
                        != Some(operation.nonce().to_be_bytes().as_slice())
                    {
                        return Err(Error::Control("public qualification KV value differs"));
                    }
                }
                "blob" => {
                    let blob = handle.blob::<fixture::ReferenceBlob>()?;
                    let key = format!("public-qualification/{run_tag}/{}", operation.index())
                        .into_bytes();
                    let upload_id = fixed_id(operation_id);
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
                    let complete_identity = identity(mutation_index.saturating_add(2), now_ms);
                    let complete = BlobMutation::Complete {
                        key: key.clone(),
                        upload_id,
                        part_count: 1,
                    };
                    let committed = blob
                        .mutate(complete_identity, complete.clone())
                        .await
                        .map_err(|_| Error::Control("public qualification Blob complete failed"))?;
                    if operation.case() == QualificationCase::Duplicate {
                        let duplicate =
                            blob.mutate(complete_identity, complete)
                                .await
                                .map_err(|source| Error::Facility {
                                    name: "public qualification Blob duplicate",
                                    source: Box::new(source),
                                })?;
                        if duplicate.receipt != committed.receipt
                            || duplicate.output != committed.output
                        {
                            return Err(Error::Control("public qualification Blob replay differs"));
                        }
                    }
                    let crab_cell_runtime::BlobMutationOutcome::Committed { etag, size } =
                        committed.output
                    else {
                        return Err(Error::Control("public qualification Blob not committed"));
                    };
                    let read = blob
                        .query(
                            BlobQuery::Read {
                                key,
                                offset: 0,
                                limit: 128,
                            },
                            None,
                        )
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Blob verification failed")
                        })?;
                    if !matches!(read.output, BlobQueryResult::Read(Some(ref blob_read))
                        if blob_read.bytes == operation.nonce().to_be_bytes()
                            && blob_read.metadata.size == size
                            && blob_read.metadata.etag == etag)
                    {
                        return Err(Error::Control("public qualification Blob bytes differ"));
                    }
                }
                "queue" => {
                    let queue = handle.queue::<fixture::ReferenceQueue>()?;
                    let producer_id = fixed_id(operation_id);
                    let send = QueueSendRequest {
                        producer_id,
                        payload: operation.nonce().to_be_bytes().to_vec(),
                        available_at_ms: now_ms,
                    };
                    let sent = queue
                        .send(mutation, send.clone())
                        .await
                        .map_err(|_| Error::Control("public qualification Queue send failed"))?;
                    let QueueSendOutcome::Sent { message_id } = sent.output else {
                        return Err(Error::Control(
                            "public qualification Queue send not applied",
                        ));
                    };
                    if operation.case() == QualificationCase::Duplicate {
                        let duplicate = queue
                            .send(identity(mutation_index.saturating_add(5), now_ms), send)
                            .await
                            .map_err(|source| Error::Facility {
                                name: "public qualification Queue duplicate",
                                source: Box::new(source),
                            })?;
                        if duplicate.output != (QueueSendOutcome::Sent { message_id }) {
                            return Err(Error::Control("public qualification Queue dedup differs"));
                        }
                    }
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
                    if message.message_id != message_id
                        || message.payload != operation.nonce().to_be_bytes()
                    {
                        return Err(Error::Control("public qualification Queue claim differs"));
                    }
                    let retried_outcome = queue
                        .retry(
                            identity(mutation_index.saturating_add(2), now_ms),
                            0,
                            message.message_id,
                            message.token,
                            0,
                        )
                        .await
                        .map_err(|source| Error::Facility {
                            name: "public qualification Queue retry",
                            source: Box::new(source),
                        })?;
                    if !matches!(
                        retried_outcome.output,
                        QueueLeaseOutcome::Applied {
                            state: QueueState::Ready,
                            ..
                        }
                    ) {
                        return Err(Error::Control(
                            "public qualification Queue retry not applied",
                        ));
                    }
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
                        .map_err(|source| Error::Facility {
                            name: "public qualification Queue reclaim",
                            source: Box::new(source),
                        })?;
                    let retried_message = retried
                        .output
                        .first()
                        .cloned()
                        .ok_or(Error::Control("public qualification Queue reclaim empty"))?;
                    if retried_message.message_id != message_id
                        || retried_message.payload != operation.nonce().to_be_bytes()
                        || retried_message.attempt <= message.attempt
                    {
                        return Err(Error::Control("public qualification Queue reclaim differs"));
                    }
                    let acked = queue
                        .ack(
                            identity(mutation_index.saturating_add(4), now_ms),
                            0,
                            retried_message.message_id,
                            retried_message.token,
                        )
                        .await
                        .map_err(|_| Error::Control("public qualification Queue ack failed"))?;
                    if !matches!(
                        acked.output,
                        QueueLeaseOutcome::Applied {
                            state: QueueState::Acked,
                            ..
                        }
                    ) {
                        return Err(Error::Control("public qualification Queue ack not applied"));
                    }
                    let info = queue
                        .info(0, Some(acked.receipt))
                        .await
                        .map_err(|_| Error::Control("public qualification Queue info failed"))?;
                    if info.output.leased != 0 || info.output.ready != 0 {
                        return Err(Error::Control("public qualification Queue work remains"));
                    }
                }
                "cron" => {
                    let cron = handle.cron::<fixture::ReferenceCron>()?;
                    let schedule_id = fixed_id(operation_id);
                    let upsert = CronMutation::Upsert {
                        schedule_id,
                        target_index: 0,
                        target_partition: partition_for_shard(0).to_vec(),
                        payload: operation.nonce().to_be_bytes().to_vec(),
                        interval_ms: 1_000,
                        next_due_ms: now_ms + 1_000,
                    };
                    let scheduled = cron
                        .mutate(mutation, upsert.clone())
                        .await
                        .map_err(|_| Error::Control("public qualification Cron upsert failed"))?;
                    if operation.case() == QualificationCase::Duplicate {
                        let duplicate = cron.mutate(mutation, upsert).await.map_err(|source| {
                            Error::Facility {
                                name: "public qualification Cron duplicate",
                                source: Box::new(source),
                            }
                        })?;
                        if duplicate.receipt != scheduled.receipt
                            || duplicate.output != scheduled.output
                        {
                            return Err(Error::Control("public qualification Cron replay differs"));
                        }
                    }
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
                    let observed = cron.get(schedule_id, None).await.map_err(|_| {
                        Error::Control("public qualification Cron verification failed")
                    })?;
                    if !matches!(observed.output, CronQueryResult::Get(Some(ref schedule))
                        if schedule.enabled
                            && schedule.schedule_id == schedule_id
                            && schedule.payload == operation.nonce().to_be_bytes())
                    {
                        return Err(Error::Control("public qualification Cron state differs"));
                    }
                }
                "workflow" => {
                    let workflow = handle.workflow::<fixture::ReferenceWorkflow>()?;
                    let workflow_id =
                        format!("public-workflow-{run_tag}-{}", operation.index()).into_bytes();
                    let started = workflow
                        .start(mutation, workflow_id.clone(), b"activity".to_vec())
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Workflow start failed")
                        })?;
                    let WorkflowOutcome::Applied { run_id, .. } = started.output else {
                        return Err(Error::Control("public qualification Workflow not applied"));
                    };
                    let cancelled = workflow
                        .cancel(
                            identity(mutation_index.saturating_add(1), now_ms),
                            WorkflowSignal {
                                workflow_id: workflow_id.clone(),
                                run_id,
                                signal_id: fixed_id(operation_id),
                                event: b"cancel".to_vec(),
                            },
                        )
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Workflow cancel failed")
                        })?;
                    if !matches!(
                        cancelled.output,
                        WorkflowOutcome::Applied {
                            status: WorkflowStatus::Cancelled,
                            ..
                        }
                    ) {
                        return Err(Error::Control(
                            "public qualification Workflow not cancelled",
                        ));
                    }
                    let observed = workflow
                        .state(workflow_id, Some(cancelled.receipt))
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Workflow verification failed")
                        })?;
                    if !matches!(observed.output, Some(ref run)
                        if run.run_id == run_id && run.status == WorkflowStatus::Cancelled)
                    {
                        return Err(Error::Control(
                            "public qualification Workflow state differs",
                        ));
                    }
                }
                "activity" => {
                    let workflow = handle.workflow::<fixture::ReferenceWorkflow>()?;
                    let workflow_id =
                        format!("public-activity-{run_tag}-{}", operation.index()).into_bytes();
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
                    let outcome = supervisor.run_once(0, None).await.map_err(|_| {
                        Error::Control("public qualification Activity invocation failed")
                    })?;
                    if !matches!(
                        outcome,
                        ActivityRunOutcome::Completed {
                            workflow: WorkflowOutcome::Applied {
                                status: WorkflowStatus::Completed,
                                ..
                            },
                            ..
                        }
                    ) {
                        return Err(Error::Control(
                            "public qualification Activity not completed",
                        ));
                    }
                    let observed =
                        workflow
                            .state(workflow_id.clone(), None)
                            .await
                            .map_err(|_| {
                                Error::Control("public qualification Activity verification failed")
                            })?;
                    if !matches!(observed.output, Some(ref run)
                        if run.status == WorkflowStatus::Completed
                            && run.workflow_id == workflow_id
                            && run.result.as_deref().is_some_and(|result|
                                result.starts_with(b"activity\0")
                                    && result.ends_with(b"activity-result")))
                    {
                        return Err(Error::Control(
                            "public qualification Activity state differs",
                        ));
                    }
                }
                "effects" => {
                    let workflow = handle.workflow::<fixture::ReferenceWorkflow>()?;
                    let workflow_id =
                        format!("public-effect-{run_tag}-{}", operation.index()).into_bytes();
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
                    let validated = effects
                        .validate(vec![claim.clone()], claims.receipt)
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Effects validation failed")
                        })?;
                    if !validated.output {
                        return Err(Error::Control("public qualification Effects lease invalid"));
                    }
                    let acked = effects
                        .ack(
                            identity(mutation_index.saturating_add(2), now_ms),
                            claim.clone(),
                            b"public-effect-result".to_vec(),
                        )
                        .await
                        .map_err(|_| Error::Control("public qualification Effects ack failed"))?;
                    if acked.output != EffectLeaseOutcome::Delivered {
                        return Err(Error::Control("public qualification Effects not delivered"));
                    }
                    let settled =
                        effects
                            .validate(vec![claim], acked.receipt)
                            .await
                            .map_err(|_| {
                                Error::Control("public qualification Effects settlement failed")
                            })?;
                    if settled.output {
                        return Err(Error::Control("public qualification Effects lease remains"));
                    }
                }
                _ => {
                    return Err(Error::Control(
                        "public qualification primitive is not registered",
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
            let execution = QualificationExecution::acknowledged(true);
            match (operation.primitive(), operation.case()) {
                (_, QualificationCase::Happy)
                | ("sql" | "kv" | "blob" | "queue" | "cron", QualificationCase::Duplicate) => {
                    Ok(execution.with_case(operation.case()))
                }
                _ => Ok(execution),
            }
        })
    }
}

async fn verify_public_kv_expiry(
    handle: &ApplicationHandle<fixture::ReferenceApplication>,
) -> Result<()> {
    let kv = handle.kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)?;
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Control("public qualification clock failed"))?
            .as_millis(),
    )
    .map_err(|_| Error::Control("public qualification clock overflow"))?;
    let scope = b"public-qualification-expiry".to_vec();
    let key = fixed_id(now_ms as u64).to_vec();
    // Keep the TTL longer than a normal provider round trip so the first read
    // proves a stored value existed before the expiry check.
    let expires_at_ms = now_ms.saturating_add(5_000);
    let created = kv
        .atomic(
            identity(now_ms as u64, now_ms),
            KvAtomicRequest {
                scope: scope.clone(),
                checks: Vec::new(),
                mutations: vec![KvMutation::Put {
                    key: key.clone(),
                    value: b"expired".to_vec(),
                    expires_at_ms: Some(expires_at_ms),
                }],
            },
        )
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification KV TTL write",
            source: Box::new(source),
        })?;
    if !matches!(created.output, KvAtomicOutcome::Applied(_)) {
        return Err(Error::Control("public qualification KV TTL not applied"));
    }
    let before_expiry = kv
        .get(scope.clone(), key.clone(), Some(created.receipt))
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification KV TTL read",
            source: Box::new(source),
        })?;
    if before_expiry
        .output
        .as_ref()
        .map(|entry| entry.value.as_slice())
        != Some(b"expired".as_slice())
    {
        return Err(Error::Control("public qualification KV TTL value missing"));
    }
    let wait_ms = u64::try_from(expires_at_ms.saturating_sub(now_ms))
        .map_err(|_| Error::Control("public qualification KV TTL wait overflow"))?;
    tokio::time::sleep(Duration::from_millis(wait_ms.saturating_add(100))).await;
    let expired = kv
        .get(scope, key, None)
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification KV TTL expired read",
            source: Box::new(source),
        })?;
    if expired.output.is_some() {
        return Err(Error::Control("public qualification KV expiry visible"));
    }
    Ok(())
}

async fn public_host_fixture() -> (
    CellNode,
    ApplicationHandle<fixture::ReferenceApplication>,
    TenantId,
    ApplicationId,
    tempfile::TempDir,
) {
    public_host_fixture_with_store(
        Store::new(Arc::new(InMemory::new())),
        Path::from("public-host-qualification"),
    )
    .await
}

async fn public_host_fixture_with_store(
    store: Store,
    root: Path,
) -> (
    CellNode,
    ApplicationHandle<fixture::ReferenceApplication>,
    TenantId,
    ApplicationId,
    tempfile::TempDir,
) {
    let application = Arc::new(fixture::compiled());
    let tenant = TenantId::from_bytes([71; 16]);
    let application_id = ApplicationId::from_bytes([72; 16]);
    let layout = CellStorageLayout::new(store.clone(), root, *application_id.as_bytes());
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
    let cancellation = CancellationToken::new();
    let tasks = node
        .install_task_group(cancellation.clone(), CancellationToken::new())
        .expect("qualification task group");
    let lease = NodeLeaseGuard::new(0, 60_000).expect("qualification lease");
    node.install_node_lease(lease.clone())
        .expect("qualification readiness");
    // This smoke has no authority publisher; keep its test lease live until
    // the node-owned task group cancels and joins the renewal loop on drain.
    tasks
        .spawn(async move {
            loop {
                tokio::select! {
                    () = cancellation.cancelled() => return Ok::<(), Error>(()),
                    () = tokio::time::sleep(Duration::from_secs(20)) => lease.renew(0, 60_000)?,
                }
            }
        })
        .expect("qualification lease renewal task");
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
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE qualification_rows (id INTEGER PRIMARY KEY, payload BLOB NOT NULL)",
                )?;
                Ok(())
            },
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
    let typed = node
        .application_handle::<fixture::ReferenceApplication>(client, tenant, application_id)
        .with_blob_artifact_store(BlobArtifactStore::new(store));
    (node, typed, tenant, application_id, directory)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_cell_node_runs_typed_primitive_workload() {
    run_public_typed_primitive_workload(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_cell_node_runs_typed_primitive_workload() {
    let (store, root) = rustfs_public_store();
    run_public_typed_primitive_workload(public_host_fixture_with_store(store, root).await).await;
}

async fn run_public_typed_primitive_workload(
    (node, typed, tenant, application_id, _directory): (
        CellNode,
        ApplicationHandle<fixture::ReferenceApplication>,
        TenantId,
        ApplicationId,
        tempfile::TempDir,
    ),
) {
    let profile = QualificationProfile::pr_contract();
    let workload = QualificationWorkload::generate_with_size(&profile, 41, 1, 64, 1)
        .expect("qualification workload");
    let mut executor = PublicHostSmokeExecutor {
        handle: typed.clone(),
        tenant,
        application: application_id,
        run_tag: 0,
    };
    assert!(node.is_ready());
    let summary = workload.run(&mut executor).await.expect("typed workload");
    assert!(node.is_ready());
    assert_eq!(summary.operations(), 64);
    assert!(summary.primitive_counts().iter().all(|counts| {
        counts.attempted() > 0
            && counts.acknowledged() == counts.attempted()
            && counts.verified() == counts.attempted()
            && counts.rejected() == 0
            && counts.ambiguous() == 0
            && counts.retried() == 0
    }));
    let covered = summary
        .case_coverage()
        .iter()
        .map(|byte| byte.count_ones())
        .sum::<u32>();
    assert_eq!(covered, 13);
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

    verify_public_kv_expiry(&typed)
        .await
        .expect("public KV expiry verification");

    drop(typed);
    node.shutdown().await.expect("qualification shutdown");
    assert!(!node.is_ready());
    assert_zero_reservations(&node);
}

struct MatrixRowEvidence {
    workload: String,
    receipt: QualificationReceipt,
    artifacts: Vec<Vec<u8>>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_cell_node_runs_complete_matrix_through_typed_apis() {
    let (node, typed, tenant, application_id, _directory) = public_host_fixture().await;
    let profile = QualificationProfile::pr_contract();
    let image = Digest::from_bytes([74; 32]);
    let signing_key_bytes = [76; 32];
    let trusted_signer = SigningKey::from_bytes(&signing_key_bytes)
        .verifying_key()
        .to_bytes();
    let mut rows = Vec::with_capacity(QUALIFICATION_MATRIX_ROWS.len());

    for (row_index, row) in QUALIFICATION_MATRIX_ROWS.iter().enumerate() {
        let workload = QualificationWorkload::generate_with_size(
            &profile,
            41_u64.saturating_add(row_index as u64),
            1,
            16,
            1,
        )
        .expect("qualification workload");
        let mut executor = PublicHostSmokeExecutor {
            handle: typed.clone(),
            tenant,
            application: application_id,
            run_tag: row_index as u64 + 1,
        };
        assert!(node.is_ready());
        let summary = workload.run(&mut executor).await.expect("typed workload");
        assert!(node.is_ready());
        let run_artifact = summary.artifact(&workload).expect("run artifact");
        run_artifact
            .verify_for_profile(&profile)
            .expect("run artifact profile");
        let workload_bytes = workload.encode().expect("workload encoding");
        let run_bytes = run_artifact.encode().expect("run artifact encoding");
        let artifacts = if *row == "primitives" {
            vec![workload_bytes, run_bytes]
        } else {
            vec![run_bytes]
        };
        let artifact_digests = artifacts
            .iter()
            .map(|artifact| Digest::from_bytes(*blake3::hash(artifact).as_bytes()))
            .collect();
        let receipt = QualificationRunner::new(SigningKey::from_bytes(&signing_key_bytes))
            .emit_with_profile_and_evidence(
                &profile,
                "public-host-matrix-source".into(),
                image,
                "local".into(),
                (*row).into(),
                "none".into(),
                summary.metrics().expect("receipt metrics"),
                artifacts
                    .first()
                    .expect("matrix row has a primary artifact"),
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
                b"none",
                artifact_digests,
                Vec::new(),
            )
            .expect("qualification receipt");
        receipt.encode().expect("receipt encoding");
        rows.push(MatrixRowEvidence {
            workload: (*row).into(),
            receipt,
            artifacts,
        });
    }

    let manifest = QualificationMatrixManifest::new(
        rows.iter()
            .map(|row| {
                QualificationMatrixEntry::new(
                    row.workload.clone(),
                    format!("receipts/{}.json", row.workload),
                    row.artifacts
                        .iter()
                        .enumerate()
                        .map(|(index, _)| format!("artifacts/{}-{index}.json", row.workload))
                        .collect(),
                )
            })
            .collect::<Result<Vec<_>>>()
            .expect("qualification matrix manifest"),
    )
    .expect("complete qualification matrix");
    let encoded_manifest = manifest.encode().expect("matrix manifest encoding");
    assert_eq!(
        QualificationMatrixManifest::decode(&encoded_manifest).expect("matrix manifest decoding"),
        manifest
    );

    let artifact_views = rows
        .iter()
        .map(|row| row.artifacts.iter().map(Vec::as_slice).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let evidence = rows
        .iter()
        .zip(artifact_views.iter())
        .map(|(row, artifacts)| (row.workload.as_str(), &row.receipt, artifacts.as_slice()))
        .collect::<Vec<_>>();
    QualificationReceipt::verify_matrix_for_profile_with_signer(
        "public-host-matrix-source",
        image,
        &profile,
        &evidence,
        trusted_signer,
    )
    .expect("typed ten-row qualification matrix");

    drop(typed);
    node.shutdown().await.expect("qualification shutdown");
    assert!(!node.is_ready());
}
