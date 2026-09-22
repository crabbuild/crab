use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_app::ApplicationHandle;
use crab_cell_host::CellNode;
use crab_cell_runtime::{
    ActivityRunOutcome, ActivitySupervisor, ApplicationId, BlobArtifactStore, BlobCondition,
    BlobMutation, BlobQuery, BlobQueryResult, CellClient, CellHandle, CellTarget, CronMutation,
    CronQueryResult, Digest, EffectClaimRequest, EffectLeaseOutcome, Error, KvAtomicOutcome,
    KvAtomicRequest, KvMutation, QUALIFICATION_CASE_COVERAGE_OPERATIONS, QUALIFICATION_MATRIX_ROWS,
    QUALIFICATION_PRIMITIVES, QualificationCase, QualificationExecution, QualificationMatrixEntry,
    QualificationMatrixManifest, QualificationOperation, QualificationOperationExecutor,
    QualificationProfile, QualificationReceipt, QualificationRunner, QualificationWorkload,
    QueueClaimRequest, QueueLeaseOutcome, QueueSendOutcome, QueueSendRequest, QueueState, Registry,
    Result, SqlBatch, SqlStatement, SqlValue, TenantId, WorkflowOutcome, WorkflowSignal,
    WorkflowStatus, partition_for_shard,
};
use crab_storage::Store;
use ed25519_dalek::SigningKey;

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_activity_duplicate.rs"]
mod qualification_activity_duplicate;
#[path = "support/qualification_cancellation.rs"]
mod qualification_cancellation;
#[path = "support/qualification_fixture.rs"]
mod qualification_fixture;
#[path = "support/qualification_local_fixture.rs"]
mod qualification_local_fixture;
#[path = "support/qualification_peer.rs"]
#[expect(dead_code, reason = "this test uses the paused peer fault helper")]
mod qualification_peer;
#[path = "support/qualification_scheduled_cancellation.rs"]
mod qualification_scheduled_cancellation;
#[path = "support/qualification_scheduled_cancellation_leases.rs"]
mod qualification_scheduled_cancellation_leases;
#[path = "support/qualification_scheduled_expiry.rs"]
mod qualification_scheduled_expiry;
#[path = "support/qualification_scheduled_retry.rs"]
mod qualification_scheduled_retry;
#[path = "support/qualification_scheduled_retry_leases.rs"]
mod qualification_scheduled_retry_leases;

use qualification::{assert_zero_reservations, fixed_id, identity, rustfs_public_store};
use qualification_fixture::{PublicHostFixture, public_host_fixture_with_store};
use qualification_local_fixture::public_host_fixture;
use qualification_peer::{
    peer_client_with_delayed_mutation_receive, peer_client_with_one_lost_mutation,
    peer_client_with_paused_mutation,
};

struct PublicHostSmokeExecutor<'n> {
    node: &'n CellNode,
    handle: ApplicationHandle<fixture::ReferenceApplication>,
    observer: ApplicationHandle<fixture::ReferenceApplication>,
    registry: Arc<Registry>,
    handles: Vec<CellHandle>,
    tenant: TenantId,
    application: ApplicationId,
    run_tag: u64,
}

fn operation_id(run_tag: u64, index: u64) -> u64 {
    run_tag.saturating_mul(1_000_000).saturating_add(index)
}

fn independent_observer(
    node: &CellNode,
    registry: &Arc<Registry>,
    handles: &[CellHandle],
    store: &Store,
    tenant: TenantId,
    application: ApplicationId,
) -> ApplicationHandle<fixture::ReferenceApplication> {
    let client = CellClient::local_many(Arc::clone(registry), handles.to_vec())
        .expect("independent qualification client");
    node.application_handle::<fixture::ReferenceApplication>(client, tenant, application)
        .with_blob_artifact_store(BlobArtifactStore::new(store.clone()))
}

fn smoke_executor<'n>(
    node: &'n CellNode,
    handle: ApplicationHandle<fixture::ReferenceApplication>,
    registry: &Arc<Registry>,
    handles: &[CellHandle],
    store: &Store,
    tenant: TenantId,
    application: ApplicationId,
    run_tag: u64,
) -> PublicHostSmokeExecutor<'n> {
    PublicHostSmokeExecutor {
        node,
        handle,
        observer: independent_observer(node, registry, handles, store, tenant, application),
        registry: Arc::clone(registry),
        handles: handles.to_vec(),
        tenant,
        application,
        run_tag,
    }
}

struct TimedSmokeExecutor<'n> {
    inner: PublicHostSmokeExecutor<'n>,
    maximum_latency: Duration,
}

impl QualificationOperationExecutor for TimedSmokeExecutor<'_> {
    type Future<'a>
        = Pin<Box<dyn Future<Output = Result<QualificationExecution>> + Send + 'a>>
    where
        Self: 'a;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let result = self.inner.execute(operation).await;
            let elapsed = started.elapsed();
            if elapsed > self.maximum_latency {
                eprintln!(
                    "slow qualification operation: primitive={} case={} index={} elapsed_ms={} result={result:?}",
                    operation.primitive(),
                    operation.case().name(),
                    operation.index(),
                    elapsed.as_millis()
                );
            }
            result
        })
    }
}

impl QualificationOperationExecutor for PublicHostSmokeExecutor<'_> {
    type Future<'a>
        = Pin<Box<dyn Future<Output = Result<QualificationExecution>> + Send + 'a>>
    where
        Self: 'a;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        let handle = self.handle.clone();
        let observer = self.observer.clone();
        let node = self.node;
        let registry = &self.registry;
        let handles = &self.handles;
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
            if operation.case() == QualificationCase::Retry {
                let (client, dropped, dispatched) = peer_client_with_one_lost_mutation(
                    Arc::clone(registry),
                    handles.to_vec(),
                    true,
                    1,
                );
                let peer = node.application_handle::<fixture::ReferenceApplication>(
                    client,
                    tenant,
                    application,
                );
                let retry = qualification_scheduled_retry::RetryCase::new(
                    &peer,
                    &observer,
                    &dropped,
                    &dispatched,
                    tenant,
                    application,
                );
                match operation.primitive() {
                    "sql" => retry.sql(sql_row_id, operation.nonce(), mutation).await?,
                    "kv" => retry.kv(operation_id, operation.nonce(), mutation).await?,
                    "blob" => {
                        retry
                            .blob(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "queue" => {
                        retry
                            .queue(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "cron" => {
                        retry
                            .cron(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "workflow" => {
                        retry
                            .workflow(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "activity" => {
                        retry
                            .activity(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "effects" => {
                        retry
                            .effects(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    _ => return Err(Error::Control("public qualification retry primitive")),
                }
                return Ok(QualificationExecution::acknowledged(true)
                    .with_retries(1)
                    .with_case(QualificationCase::Retry));
            }
            if operation.case() == QualificationCase::Cancellation
                && matches!(
                    operation.primitive(),
                    "sql" | "kv" | "blob" | "queue" | "cron" | "workflow" | "activity" | "effects"
                )
            {
                let (client, entered, dispatched) =
                    peer_client_with_paused_mutation(Arc::clone(registry), handles.to_vec(), true);
                let peer = node.application_handle::<fixture::ReferenceApplication>(
                    client,
                    tenant,
                    application,
                );
                let cancellation = qualification_scheduled_cancellation::CancellationCase::new(
                    &peer,
                    &observer,
                    entered,
                    &dispatched,
                    tenant,
                    application,
                );
                match operation.primitive() {
                    "sql" => {
                        cancellation
                            .sql(sql_row_id, operation.nonce(), mutation)
                            .await?
                    }
                    "kv" => {
                        cancellation
                            .kv(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "blob" => {
                        cancellation
                            .blob(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "queue" => {
                        cancellation
                            .queue(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "cron" => {
                        cancellation
                            .cron(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "workflow" => {
                        cancellation
                            .workflow(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "activity" => {
                        cancellation
                            .activity(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    "effects" => {
                        cancellation
                            .effects(operation_id, operation.nonce(), mutation)
                            .await?
                    }
                    _ => {
                        return Err(Error::Control(
                            "public qualification cancellation primitive",
                        ));
                    }
                }
                return Ok(QualificationExecution::acknowledged(true)
                    .with_case(QualificationCase::Cancellation));
            }
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
                                lease_ms: 60_000,
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
                                lease_ms: 60_000,
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
                        .map_err(|source| Error::Facility {
                            name: "public qualification Queue ack",
                            source: Box::new(source),
                        })?;
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
                    let scheduled =
                        cron.mutate(mutation, upsert.clone())
                            .await
                            .map_err(|source| Error::Facility {
                                name: "public qualification Cron upsert",
                                source: Box::new(source),
                            })?;
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
                    .map_err(|source| Error::Facility {
                        name: "public qualification Cron pause",
                        source: Box::new(source),
                    })?;
                    cron.mutate(
                        identity(mutation_index.saturating_add(2), now_ms),
                        CronMutation::Resume {
                            schedule_id,
                            next_due_ms: now_ms + 1_000,
                        },
                    )
                    .await
                    .map_err(|source| Error::Facility {
                        name: "public qualification Cron resume",
                        source: Box::new(source),
                    })?;
                    let observed =
                        cron.get(schedule_id, None)
                            .await
                            .map_err(|source| Error::Facility {
                                name: "public qualification Cron verification",
                                source: Box::new(source),
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
                    if operation.case() == QualificationCase::Duplicate {
                        let duplicate = workflow
                            .start(mutation, workflow_id.clone(), b"activity".to_vec())
                            .await
                            .map_err(|source| Error::Facility {
                                name: "public qualification Workflow duplicate start",
                                source: Box::new(source),
                            })?;
                        if duplicate.receipt != started.receipt
                            || duplicate.output != started.output
                        {
                            return Err(Error::Control(
                                "public qualification Workflow start replay differs",
                            ));
                        }
                    }
                    let cancel_identity = identity(mutation_index.saturating_add(1), now_ms);
                    let signal = WorkflowSignal {
                        workflow_id: workflow_id.clone(),
                        run_id,
                        signal_id: fixed_id(operation_id),
                        event: b"cancel".to_vec(),
                    };
                    let cancelled = workflow
                        .cancel(cancel_identity, signal.clone())
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
                    if operation.case() == QualificationCase::Duplicate {
                        let duplicate =
                            workflow
                                .cancel(cancel_identity, signal)
                                .await
                                .map_err(|source| Error::Facility {
                                    name: "public qualification Workflow duplicate cancel",
                                    source: Box::new(source),
                                })?;
                        if duplicate.receipt != cancelled.receipt
                            || duplicate.output != cancelled.output
                        {
                            return Err(Error::Control(
                                "public qualification Workflow cancel replay differs",
                            ));
                        }
                    }
                    let observed = workflow
                        .state(workflow_id, Some(cancelled.receipt))
                        .await
                        .map_err(|_| {
                            Error::Control("public qualification Workflow verification failed")
                        })?;
                    if !matches!(observed.output, Some(ref run)
                        if run.run_id == run_id
                            && run.status == WorkflowStatus::Cancelled
                            && matches!(cancelled.output, WorkflowOutcome::Applied {
                                event_sequence, ..
                            } if run.event_sequence == event_sequence))
                    {
                        return Err(Error::Control(
                            "public qualification Workflow state differs",
                        ));
                    }
                }
                "activity" => {
                    if operation.case() == QualificationCase::Duplicate {
                        qualification_activity_duplicate::run(
                            &handle,
                            &observer,
                            tenant,
                            application,
                            operation_id,
                            operation.nonce(),
                            now_ms,
                        )
                        .await?;
                    } else {
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
                            60_000,
                        )?;
                        let outcome = supervisor.run_once(0, None).await.map_err(|source| {
                            Error::Facility {
                                name: "public qualification Activity invocation",
                                source: Box::new(source),
                            }
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
                                    Error::Control(
                                        "public qualification Activity verification failed",
                                    )
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
                                lease_ms: 60_000,
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
                    let ack_identity = identity(mutation_index.saturating_add(2), now_ms);
                    let acked = effects
                        .ack(
                            ack_identity,
                            claim.clone(),
                            b"public-effect-result".to_vec(),
                        )
                        .await
                        .map_err(|source| Error::Facility {
                            name: "public qualification Effects ack",
                            source: Box::new(source),
                        })?;
                    if acked.output != EffectLeaseOutcome::Delivered {
                        return Err(Error::Control("public qualification Effects not delivered"));
                    }
                    if operation.case() == QualificationCase::Duplicate {
                        let duplicate = effects
                            .ack(
                                ack_identity,
                                claim.clone(),
                                b"public-effect-result".to_vec(),
                            )
                            .await
                            .map_err(|source| Error::Facility {
                                name: "public qualification Effects duplicate ack",
                                source: Box::new(source),
                            })?;
                        if duplicate.receipt != acked.receipt || duplicate.output != acked.output {
                            return Err(Error::Control(
                                "public qualification Effects ack replay differs",
                            ));
                        }
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
                | (
                    "sql" | "kv" | "blob" | "queue" | "cron" | "workflow" | "activity" | "effects",
                    QualificationCase::Duplicate,
                ) => Ok(execution.with_case(operation.case())),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_activity_duplicate_case_preserves_one_completion() {
    let (store, root) = rustfs_public_store();
    let (node, typed, tenant, application, _directory, registry, handles, store) =
        public_host_fixture_with_store(store, root).await;
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        QUALIFICATION_CASE_COVERAGE_OPERATIONS,
        1,
    )
    .expect("qualification case schedule");
    let operation = workload
        .iter_operations()
        .find(|operation| {
            operation.primitive() == "activity" && operation.case() == QualificationCase::Duplicate
        })
        .expect("scheduled Activity duplicate case");
    let mut executor = smoke_executor(
        &node,
        typed.clone(),
        &registry,
        &handles,
        &store,
        tenant,
        application,
        0,
    );
    let execution = executor
        .execute(operation)
        .await
        .expect("Activity duplicate case");
    assert!(execution.verified());
    assert_eq!(execution.case(), Some(QualificationCase::Duplicate));
    drop(executor);
    drop(typed);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_sql_cancellation_case_preserves_acknowledged_row() {
    run_scheduled_sql_cancellation_case(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_sql_cancellation_case_preserves_acknowledged_row() {
    let (store, root) = rustfs_public_store();
    run_scheduled_sql_cancellation_case(public_host_fixture_with_store(store, root).await).await;
}

async fn run_scheduled_sql_cancellation_case(
    (node, typed, tenant, application, _directory, registry, handles, store): PublicHostFixture,
) {
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        QUALIFICATION_CASE_COVERAGE_OPERATIONS,
        1,
    )
    .expect("qualification case schedule");
    let operation = workload
        .iter_operations()
        .find(|operation| {
            operation.primitive() == "sql" && operation.case() == QualificationCase::Cancellation
        })
        .expect("scheduled SQL cancellation case");
    let mut executor = smoke_executor(
        &node,
        typed.clone(),
        &registry,
        &handles,
        &store,
        tenant,
        application,
        0,
    );
    let execution = executor
        .execute(operation)
        .await
        .expect("SQL cancellation case");
    assert!(execution.verified());
    assert_eq!(execution.case(), Some(QualificationCase::Cancellation));
    drop(executor);
    drop(typed);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_kv_cron_workflow_cancellation_cases_preserve_acknowledged_results() {
    run_scheduled_cancellation_cases(public_host_fixture().await, &["kv", "cron", "workflow"])
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_kv_cron_workflow_cancellation_cases_preserve_acknowledged_results() {
    let (store, root) = rustfs_public_store();
    run_scheduled_cancellation_cases(
        public_host_fixture_with_store(store, root).await,
        &["kv", "cron", "workflow"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_blob_queue_activity_effect_cancellation_cases_preserve_acknowledged_results() {
    run_scheduled_cancellation_cases(
        public_host_fixture().await,
        &["blob", "queue", "activity", "effects"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_blob_queue_activity_effect_cancellation_cases_preserve_acknowledged_results()
{
    let (store, root) = rustfs_public_store();
    run_scheduled_cancellation_cases(
        public_host_fixture_with_store(store, root).await,
        &["blob", "queue", "activity", "effects"],
    )
    .await;
}

async fn run_scheduled_cancellation_cases(
    (node, typed, tenant, application, _directory, registry, handles, store): PublicHostFixture,
    primitives: &[&str],
) {
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        QUALIFICATION_CASE_COVERAGE_OPERATIONS,
        1,
    )
    .expect("qualification case schedule");
    let mut executor = smoke_executor(
        &node,
        typed.clone(),
        &registry,
        &handles,
        &store,
        tenant,
        application,
        0,
    );
    for &primitive in primitives {
        let operation = workload
            .iter_operations()
            .find(|operation| {
                operation.primitive() == primitive
                    && operation.case() == QualificationCase::Cancellation
            })
            .expect("scheduled cancellation case");
        let execution = executor
            .execute(operation)
            .await
            .expect("cancellation case");
        assert!(
            execution.verified(),
            "{primitive} cancellation not verified"
        );
        assert_eq!(execution.case(), Some(QualificationCase::Cancellation));
    }
    drop(executor);
    drop(typed);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_scheduled_retry_cases_preserve_exact_results() {
    run_scheduled_retry_cases(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_scheduled_retry_cases_preserve_exact_results() {
    let (store, root) = rustfs_public_store();
    run_scheduled_retry_cases(public_host_fixture_with_store(store, root).await).await;
}

async fn run_scheduled_retry_cases(
    (node, typed, tenant, application, _directory, registry, handles, store): PublicHostFixture,
) {
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        QUALIFICATION_CASE_COVERAGE_OPERATIONS,
        1,
    )
    .expect("qualification case schedule");
    let mut executor = smoke_executor(
        &node,
        typed.clone(),
        &registry,
        &handles,
        &store,
        tenant,
        application,
        0,
    );
    for &primitive in QUALIFICATION_PRIMITIVES {
        let operation = workload
            .iter_operations()
            .find(|operation| {
                operation.primitive() == primitive && operation.case() == QualificationCase::Retry
            })
            .expect("scheduled retry case");
        let execution = executor.execute(operation).await.expect("retry case");
        assert!(execution.verified(), "{primitive} retry not verified");
        assert_eq!(execution.retries(), 1);
        assert_eq!(execution.case(), Some(QualificationCase::Retry));
    }
    drop(executor);
    drop(typed);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_scheduled_workflow_expiry_rejects_delayed_mutation() {
    run_scheduled_expiry_case(public_host_fixture().await, "workflow").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_scheduled_workflow_expiry_rejects_delayed_mutation() {
    let (store, root) = rustfs_public_store();
    run_scheduled_expiry_case(
        public_host_fixture_with_store(store, root).await,
        "workflow",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_scheduled_cron_expiry_rejects_delayed_mutation() {
    run_scheduled_expiry_case(public_host_fixture().await, "cron").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_scheduled_cron_expiry_rejects_delayed_mutation() {
    let (store, root) = rustfs_public_store();
    run_scheduled_expiry_case(public_host_fixture_with_store(store, root).await, "cron").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_scheduled_sql_expiry_rejects_delayed_mutation() {
    run_scheduled_expiry_case(public_host_fixture().await, "sql").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_scheduled_sql_expiry_rejects_delayed_mutation() {
    let (store, root) = rustfs_public_store();
    run_scheduled_expiry_case(public_host_fixture_with_store(store, root).await, "sql").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_scheduled_kv_expiry_rejects_delayed_mutation() {
    run_scheduled_expiry_case(public_host_fixture().await, "kv").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_scheduled_kv_expiry_rejects_delayed_mutation() {
    let (store, root) = rustfs_public_store();
    run_scheduled_expiry_case(public_host_fixture_with_store(store, root).await, "kv").await;
}

async fn run_scheduled_expiry_case(
    (node, writer, tenant, application, _directory, registry, handles, store): PublicHostFixture,
    primitive: &str,
) {
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        QUALIFICATION_CASE_COVERAGE_OPERATIONS,
        1,
    )
    .expect("qualification case schedule");
    let operation = workload
        .iter_operations()
        .find(|operation| {
            operation.primitive() == primitive && operation.case() == QualificationCase::Expiry
        })
        .expect("scheduled expiry case");
    let observer = independent_observer(&node, &registry, &handles, &store, tenant, application);
    let (client, entered, release, dispatched) =
        peer_client_with_delayed_mutation_receive(registry, handles);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(client, tenant, application);
    let case = qualification_scheduled_expiry::ExpiryCase::new(
        &writer,
        &peer,
        &observer,
        &entered,
        &release,
        &dispatched,
        tenant,
        application,
    );
    let result = match primitive {
        "workflow" => case.workflow(operation.index(), operation.nonce()).await,
        "cron" => case.cron(operation.index(), operation.nonce()).await,
        "sql" => case
            .sql(operation.index(), operation.nonce())
            .await
            .map(|_| ()),
        "kv" => case
            .kv(operation.index(), operation.nonce())
            .await
            .map(|_| ()),
        _ => Err(Error::Control("unknown scheduled expiry primitive")),
    };
    drop(writer);
    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
    result.unwrap_or_else(|error| panic!("scheduled {primitive} expiry: {error:?}"));
}

async fn run_public_typed_primitive_workload(
    (node, typed, tenant, application_id, _directory, registry, handles, store): PublicHostFixture,
) {
    let profile = QualificationProfile::pr_contract();
    let workload = QualificationWorkload::generate_with_size(&profile, 41, 1, 64, 1)
        .expect("qualification workload");
    let mut executor = TimedSmokeExecutor {
        inner: smoke_executor(
            &node,
            typed.clone(),
            &registry,
            &handles,
            &store,
            tenant,
            application_id,
            0,
        ),
        maximum_latency: Duration::from_millis(profile.maximum_p99_latency_ms()),
    };
    assert!(node.is_ready());
    let summary = match node
        .run_qualification_observed(&workload, &mut executor)
        .await
    {
        Ok(summary) => summary,
        Err(error) => {
            drop(executor);
            drop(typed);
            node.shutdown().await.expect("qualification shutdown");
            assert_zero_reservations(&node);
            panic!("typed workload: {error:?}");
        }
    };
    assert!(node.is_ready());
    assert_eq!(summary.operations(), 64);
    assert!(summary.primitive_counts().iter().all(|counts| {
        counts.attempted() > 0
            && counts.acknowledged() == counts.attempted()
            && counts.verified() == counts.attempted()
            && counts.rejected() == 0
            && counts.ambiguous() == 0
            && counts.retried() > 0
    }));
    let covered = summary
        .case_coverage()
        .iter()
        .map(|byte| byte.count_ones())
        .sum::<u32>();
    let observed_smoke_cases = (QUALIFICATION_PRIMITIVES.len() * 4) as u32;
    assert_eq!(covered, observed_smoke_cases);
    assert!(covered < QUALIFICATION_CASE_COVERAGE_OPERATIONS as u32);
    let artifact = summary.artifact(&workload).expect("run artifact");
    if let Err(error) = artifact.verify_for_profile(&profile) {
        let metrics = summary.metrics().expect("measured qualification metrics");
        drop(executor);
        drop(typed);
        node.shutdown().await.expect("qualification shutdown");
        assert_zero_reservations(&node);
        panic!(
            "run artifact profile: {error}; measured metrics: {:?}",
            metrics
        );
    }
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
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_protocol_matrix_row_meets_profile() {
    let (store, root) = rustfs_public_store();
    let (node, typed, tenant, application, _directory, registry, handles, store) =
        public_host_fixture_with_store(store, root).await;
    let profile = QualificationProfile::pr_contract();
    let workload = QualificationWorkload::generate_with_size(&profile, 41, 1, 64, 1)
        .expect("protocol row workload");
    let mut executor = TimedSmokeExecutor {
        inner: smoke_executor(
            &node,
            typed.clone(),
            &registry,
            &handles,
            &store,
            tenant,
            application,
            1,
        ),
        maximum_latency: Duration::from_millis(profile.maximum_p99_latency_ms()),
    };
    let summary = match node
        .run_qualification_observed(&workload, &mut executor)
        .await
    {
        Ok(summary) => summary,
        Err(error) => {
            drop(executor);
            drop(typed);
            node.shutdown().await.expect("qualification shutdown");
            assert_zero_reservations(&node);
            panic!("protocol row typed workload: {error:?}");
        }
    };
    let artifact = summary.artifact(&workload).expect("protocol row artifact");
    let profile_error = artifact.verify_for_profile(&profile).err().map(|error| {
        format!(
            "protocol row profile: {error}; measured metrics: {:?}",
            summary.metrics().expect("protocol row metrics")
        )
    });
    drop(executor);
    drop(typed);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
    if let Some(error) = profile_error {
        panic!("{error}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_cell_node_runs_complete_matrix_through_typed_apis() {
    run_public_typed_matrix(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_cell_node_runs_complete_matrix_through_typed_apis() {
    let (store, root) = rustfs_public_store();
    run_public_typed_matrix(public_host_fixture_with_store(store, root).await).await;
}

async fn run_public_typed_matrix(
    (node, typed, tenant, application_id, _directory, registry, handles, store): PublicHostFixture,
) {
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
            // Keep each row above the PR profile's measured one-second floor without sleeps.
            64,
            1,
        )
        .expect("qualification workload");
        let mut executor = TimedSmokeExecutor {
            inner: smoke_executor(
                &node,
                typed.clone(),
                &registry,
                &handles,
                &store,
                tenant,
                application_id,
                row_index as u64 + 1,
            ),
            maximum_latency: Duration::from_millis(profile.maximum_p99_latency_ms()),
        };
        assert!(node.is_ready());
        let summary = match node
            .run_qualification_observed(&workload, &mut executor)
            .await
        {
            Ok(summary) => summary,
            Err(error) => {
                drop(executor);
                drop(typed);
                node.shutdown().await.expect("qualification shutdown");
                assert_zero_reservations(&node);
                panic!("matrix row {row} typed workload: {error:?}");
            }
        };
        assert!(node.is_ready());
        let run_artifact = summary.artifact(&workload).expect("run artifact");
        if let Err(error) = run_artifact.verify_for_profile(&profile) {
            let metrics = summary.metrics().expect("measured qualification metrics");
            drop(executor);
            drop(typed);
            node.shutdown().await.expect("qualification shutdown");
            assert_zero_reservations(&node);
            panic!(
                "matrix row {row} run artifact profile: {error}; measured metrics: {:?}",
                metrics
            );
        }
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
                1_001,
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
    assert_zero_reservations(&node);
}
