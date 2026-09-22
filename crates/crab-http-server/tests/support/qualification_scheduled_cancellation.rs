use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crab_cell_app::ApplicationHandle;
use crab_cell_runtime::{
    ApplicationId, CellTarget, CronCommand, CronMutation, CronMutationOutcome, CronQueryResult,
    Error, KvAtomicCommand, KvAtomicOutcome, KvAtomicRequest, KvMutation, MutationIdentity,
    Resolution, Result, SqlBatch, SqlBatchCommand, SqlResultSet, SqlStatement, SqlValue, TenantId,
    WorkflowOutcome, WorkflowStart, WorkflowStartCommand, WorkflowStatus, partition_for_shard,
};
use tokio::sync::Notify;

use crate::{
    fixture,
    qualification::fixed_id,
    qualification_cancellation::{cancel_prepared, committed_output},
};

pub struct CancellationCase<'a> {
    peer: &'a ApplicationHandle<fixture::ReferenceApplication>,
    observer: &'a ApplicationHandle<fixture::ReferenceApplication>,
    entered: Arc<Notify>,
    dispatched: &'a Arc<AtomicUsize>,
    tenant: TenantId,
    application: ApplicationId,
}

impl<'a> CancellationCase<'a> {
    pub fn new(
        peer: &'a ApplicationHandle<fixture::ReferenceApplication>,
        observer: &'a ApplicationHandle<fixture::ReferenceApplication>,
        entered: Arc<Notify>,
        dispatched: &'a Arc<AtomicUsize>,
        tenant: TenantId,
        application: ApplicationId,
    ) -> Self {
        Self {
            peer,
            observer,
            entered,
            dispatched,
            tenant,
            application,
        }
    }

    pub async fn sql(self, row_id: i64, nonce: u64, mutation: MutationIdentity) -> Result<()> {
        let target = CellTarget::new(
            self.tenant,
            self.application,
            fixture::SQL_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let expected = SqlValue::Blob(nonce.to_be_bytes().to_vec());
        let prepared = self
            .peer
            .prepare_command::<SqlBatchCommand<fixture::ReferenceSql>>(
                &target,
                mutation,
                SqlBatch {
                    statements: vec![SqlStatement {
                        sql: "INSERT INTO qualification_rows (id, payload) VALUES (?1, ?2)".into(),
                        parameters: vec![SqlValue::Integer(row_id), expected.clone()],
                    }],
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification SQL cancellation prepare",
                source: Box::new(source),
            })?;
        let (_retained_attempt, resolution) =
            cancel_prepared(prepared, self.observer, self.entered, self.dispatched, true).await;
        let Resolution::Committed(outcome) = resolution else {
            return Err(Error::Control(
                "public qualification SQL cancelled acknowledgement unresolved",
            ));
        };
        let (output, commit_sequence) = committed_output::<Vec<SqlResultSet>>(outcome);
        if output.len() != 1
            || output[0].rows_affected != 1
            || self.dispatched.load(Ordering::Acquire) != 1
        {
            return Err(Error::Control(
                "public qualification SQL cancelled mutation differs",
            ));
        }
        let observed = self
            .observer
            .sql::<fixture::ReferenceSql>(target)?
            .query(
                None,
                SqlBatch {
                    statements: vec![SqlStatement {
                        sql: "SELECT payload FROM qualification_rows WHERE id = ?1".into(),
                        parameters: vec![SqlValue::Integer(row_id)],
                    }],
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification SQL cancellation observation",
                source: Box::new(source),
            })?;
        if observed.output.len() != 1
            || observed.output[0].rows != vec![vec![expected]]
            || observed.receipt.commit_sequence < commit_sequence
        {
            return Err(Error::Control(
                "public qualification SQL cancelled row differs",
            ));
        }
        Ok(())
    }

    pub async fn kv(self, operation_id: u64, nonce: u64, mutation: MutationIdentity) -> Result<()> {
        let target = CellTarget::new(
            self.tenant,
            self.application,
            fixture::KV_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let scope = b"public-qualification".to_vec();
        let key = operation_id.to_be_bytes().to_vec();
        let value = nonce.to_be_bytes().to_vec();
        let prepared = self
            .peer
            .prepare_command::<KvAtomicCommand<fixture::ReferenceKv>>(
                &target,
                mutation,
                KvAtomicRequest {
                    scope: scope.clone(),
                    checks: Vec::new(),
                    mutations: vec![KvMutation::Put {
                        key: key.clone(),
                        value: value.clone(),
                        expires_at_ms: None,
                    }],
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification KV cancellation prepare",
                source: Box::new(source),
            })?;
        let (_retained_attempt, resolution) =
            cancel_prepared(prepared, self.observer, self.entered, self.dispatched, true).await;
        let Resolution::Committed(outcome) = resolution else {
            return Err(Error::Control(
                "public qualification KV cancelled acknowledgement unresolved",
            ));
        };
        let (output, commit_sequence) = committed_output::<KvAtomicOutcome>(outcome);
        let KvAtomicOutcome::Applied(results) = output else {
            return Err(Error::Control(
                "public qualification KV cancelled write not applied",
            ));
        };
        let [result] = results.as_slice() else {
            return Err(Error::Control(
                "public qualification KV cancelled result differs",
            ));
        };
        let Some(version) = result.version else {
            return Err(Error::Control(
                "public qualification KV cancelled version missing",
            ));
        };
        let observed = self
            .observer
            .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)?
            .get(scope, key, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification KV cancellation observation",
                source: Box::new(source),
            })?;
        if self.dispatched.load(Ordering::Acquire) != 1
            || !matches!(observed.output, Some(ref entry)
                if entry.value == value && entry.version == version)
            || observed.receipt.commit_sequence < commit_sequence
        {
            return Err(Error::Control(
                "public qualification KV cancelled value differs",
            ));
        }
        Ok(())
    }

    pub async fn cron(
        self,
        operation_id: u64,
        nonce: u64,
        mutation: MutationIdentity,
    ) -> Result<()> {
        let target = CellTarget::new(
            self.tenant,
            self.application,
            fixture::CRON_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let schedule_id = fixed_id(operation_id);
        let payload = nonce.to_be_bytes().to_vec();
        let next_due_ms = mutation.issued_at_ms.saturating_add(60_000);
        let prepared = self
            .peer
            .prepare_command::<CronCommand<fixture::ReferenceCron>>(
                &target,
                mutation,
                CronMutation::Upsert {
                    schedule_id,
                    target_index: 0,
                    target_partition: partition_for_shard(0).to_vec(),
                    payload: payload.clone(),
                    interval_ms: 60_000,
                    next_due_ms,
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Cron cancellation prepare",
                source: Box::new(source),
            })?;
        let (_retained_attempt, resolution) =
            cancel_prepared(prepared, self.observer, self.entered, self.dispatched, true).await;
        let Resolution::Committed(outcome) = resolution else {
            return Err(Error::Control(
                "public qualification Cron cancelled acknowledgement unresolved",
            ));
        };
        let (output, commit_sequence) = committed_output::<CronMutationOutcome>(outcome);
        let observed = self
            .observer
            .cron::<fixture::ReferenceCron>()?
            .get(schedule_id, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Cron cancellation observation",
                source: Box::new(source),
            })?;
        if output != (CronMutationOutcome::Applied { generation: 1 })
            || self.dispatched.load(Ordering::Acquire) != 1
            || !matches!(observed.output, CronQueryResult::Get(Some(ref schedule))
                if schedule.schedule_id == schedule_id
                    && schedule.payload == payload
                    && schedule.generation == 1
                    && schedule.next_due_ms == next_due_ms
                    && schedule.occurrence == 0
                    && schedule.enabled)
            || observed.receipt.commit_sequence < commit_sequence
        {
            return Err(Error::Control(
                "public qualification Cron cancelled schedule differs",
            ));
        }
        Ok(())
    }

    pub async fn workflow(
        self,
        operation_id: u64,
        nonce: u64,
        mutation: MutationIdentity,
    ) -> Result<()> {
        let target = CellTarget::new(
            self.tenant,
            self.application,
            fixture::WORKFLOW_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let workflow_id =
            format!("public-workflow-cancellation-{operation_id}-{nonce}").into_bytes();
        let mut event = b"cancellation-".to_vec();
        event.extend_from_slice(&nonce.to_be_bytes());
        let prepared = self
            .peer
            .prepare_command::<WorkflowStartCommand<fixture::ReferenceWorkflow>>(
                &target,
                mutation,
                WorkflowStart {
                    workflow_id: workflow_id.clone(),
                    request_id: mutation.request_id,
                    event: event.clone(),
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Workflow cancellation prepare",
                source: Box::new(source),
            })?;
        let (_retained_attempt, resolution) =
            cancel_prepared(prepared, self.observer, self.entered, self.dispatched, true).await;
        let Resolution::Committed(outcome) = resolution else {
            return Err(Error::Control(
                "public qualification Workflow cancelled acknowledgement unresolved",
            ));
        };
        let (output, commit_sequence) = committed_output::<WorkflowOutcome>(outcome);
        let WorkflowOutcome::Applied {
            run_id,
            status: WorkflowStatus::Completed,
            event_sequence: 1,
        } = output
        else {
            return Err(Error::Control(
                "public qualification Workflow cancelled start differs",
            ));
        };
        let observed = self
            .observer
            .workflow::<fixture::ReferenceWorkflow>()?
            .state(workflow_id.clone(), None)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Workflow cancellation observation",
                source: Box::new(source),
            })?;
        if self.dispatched.load(Ordering::Acquire) != 1
            || !matches!(observed.output, Some(ref run)
                if run.run_id == run_id
                    && run.workflow_id == workflow_id
                    && run.status == WorkflowStatus::Completed
                    && run.event_sequence == 1
                    && run.result.as_deref() == Some(event.as_slice()))
            || observed.receipt.commit_sequence < commit_sequence
        {
            return Err(Error::Control(
                "public qualification Workflow cancelled state differs",
            ));
        }
        Ok(())
    }
}
