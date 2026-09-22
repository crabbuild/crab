use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crab_cell_app::ApplicationHandle;
use crab_cell_runtime::{
    ApplicationId, CellTarget, Command, Committed, CronCommand, CronMutation, CronMutationOutcome,
    CronQueryResult, Error, InvocationError, KvAtomicCommand, KvAtomicOutcome, KvAtomicRequest,
    KvMutation, MutationIdentity, Resolution, Result, SqlBatch, SqlBatchCommand, SqlStatement,
    SqlValue, TenantId, WorkflowOutcome, WorkflowStart, WorkflowStartCommand, WorkflowStatus,
    partition_for_shard,
};

use crate::{fixture, qualification::fixed_id};

pub struct RetryCase<'a> {
    pub(crate) peer: &'a ApplicationHandle<fixture::ReferenceApplication>,
    pub(crate) observer: &'a ApplicationHandle<fixture::ReferenceApplication>,
    dropped: &'a Arc<AtomicUsize>,
    pub(crate) dispatched: &'a Arc<AtomicUsize>,
    pub(crate) tenant: TenantId,
    pub(crate) application: ApplicationId,
}

impl<'a> RetryCase<'a> {
    pub fn new(
        peer: &'a ApplicationHandle<fixture::ReferenceApplication>,
        observer: &'a ApplicationHandle<fixture::ReferenceApplication>,
        dropped: &'a Arc<AtomicUsize>,
        dispatched: &'a Arc<AtomicUsize>,
        tenant: TenantId,
        application: ApplicationId,
    ) -> Self {
        Self {
            peer,
            observer,
            dropped,
            dispatched,
            tenant,
            application,
        }
    }

    pub(crate) async fn command<C: Command>(
        &self,
        target: &CellTarget,
        mutation: MutationIdentity,
        input: C::Input,
    ) -> Result<Committed<C::Output>>
    where
        C::Output: Sync,
    {
        let prepared = self
            .peer
            .prepare_command::<C>(target, mutation, input)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification retry prepare",
                source: Box::new(source),
            })?;
        let pending = match prepared.clone().execute().await {
            Err(InvocationError::Pending(pending)) => pending,
            _ => {
                return Err(Error::Control(
                    "public qualification first retry attempt not pending",
                ));
            }
        };
        if self.dropped.load(Ordering::Acquire) != 1
            || self.dispatched.load(Ordering::Acquire) != 0
            || pending.as_ref() != prepared.evidence()
        {
            return Err(Error::Control("public qualification request loss differs"));
        }
        let resolution =
            self.observer
                .resolve(&pending)
                .await
                .map_err(|source| Error::Facility {
                    name: "public qualification absent request resolution",
                    source: Box::new(source),
                })?;
        if resolution != Resolution::Absent {
            return Err(Error::Control(
                "public qualification lost request was not absent",
            ));
        }
        let retried = prepared.execute().await.map_err(|source| Error::Facility {
            name: "public qualification exact request retry",
            source: Box::new(source),
        })?;
        if self.dispatched.load(Ordering::Acquire) != 1 {
            return Err(Error::Control(
                "public qualification retry dispatch differs",
            ));
        }
        Ok(retried)
    }

    pub async fn sql(self, row_id: i64, nonce: u64, mutation: MutationIdentity) -> Result<()> {
        let target = CellTarget::new(
            self.tenant,
            self.application,
            fixture::SQL_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let payload = nonce.to_be_bytes().to_vec();
        let retried = self
            .command::<SqlBatchCommand<fixture::ReferenceSql>>(
                &target,
                mutation,
                SqlBatch {
                    statements: vec![SqlStatement {
                        sql: "INSERT INTO qualification_rows (id, payload) VALUES (?1, ?2)".into(),
                        parameters: vec![
                            SqlValue::Integer(row_id),
                            SqlValue::Blob(payload.clone()),
                        ],
                    }],
                },
            )
            .await?;
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
                name: "public qualification SQL retry observation",
                source: Box::new(source),
            })?;
        if retried.output.len() != 1
            || retried.output[0].rows_affected != 1
            || observed.output.len() != 1
            || observed.output[0].rows != vec![vec![SqlValue::Blob(payload)]]
            || observed.receipt.commit_sequence < retried.receipt.commit_sequence
        {
            return Err(Error::Control(
                "public qualification SQL retry state differs",
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
        let retried = self
            .command::<KvAtomicCommand<fixture::ReferenceKv>>(
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
            .await?;
        let KvAtomicOutcome::Applied(results) = retried.output else {
            return Err(Error::Control("public qualification KV retry not applied"));
        };
        let [result] = results.as_slice() else {
            return Err(Error::Control(
                "public qualification KV retry results differ",
            ));
        };
        let Some(version) = result.version else {
            return Err(Error::Control(
                "public qualification KV retry version missing",
            ));
        };
        let observed = self
            .observer
            .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)?
            .get(scope, key, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification KV retry observation",
                source: Box::new(source),
            })?;
        if !matches!(observed.output, Some(ref entry)
            if entry.value == value && entry.version == version)
            || observed.receipt.commit_sequence < retried.receipt.commit_sequence
        {
            return Err(Error::Control(
                "public qualification KV retry state differs",
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
        let retried = self
            .command::<CronCommand<fixture::ReferenceCron>>(
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
            .await?;
        let observed = self
            .observer
            .cron::<fixture::ReferenceCron>()?
            .get(schedule_id, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Cron retry observation",
                source: Box::new(source),
            })?;
        if retried.output != (CronMutationOutcome::Applied { generation: 1 })
            || !matches!(observed.output, CronQueryResult::Get(Some(ref schedule))
                if schedule.schedule_id == schedule_id
                    && schedule.payload == payload
                    && schedule.generation == 1
                    && schedule.next_due_ms == next_due_ms
                    && schedule.occurrence == 0
                    && schedule.enabled)
            || observed.receipt.commit_sequence < retried.receipt.commit_sequence
        {
            return Err(Error::Control(
                "public qualification Cron retry state differs",
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
        let workflow_id = format!("public-workflow-retry-{operation_id}-{nonce}").into_bytes();
        let mut event = b"retry-".to_vec();
        event.extend_from_slice(&nonce.to_be_bytes());
        let retried = self
            .command::<WorkflowStartCommand<fixture::ReferenceWorkflow>>(
                &target,
                mutation,
                WorkflowStart {
                    workflow_id: workflow_id.clone(),
                    request_id: mutation.request_id,
                    event: event.clone(),
                },
            )
            .await?;
        let WorkflowOutcome::Applied {
            run_id,
            status: WorkflowStatus::Completed,
            event_sequence: 1,
        } = retried.output
        else {
            return Err(Error::Control(
                "public qualification Workflow retry did not complete",
            ));
        };
        let observed = self
            .observer
            .workflow::<fixture::ReferenceWorkflow>()?
            .state(workflow_id.clone(), None)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Workflow retry observation",
                source: Box::new(source),
            })?;
        if !matches!(observed.output, Some(ref run)
            if run.run_id == run_id
                && run.workflow_id == workflow_id
                && run.status == WorkflowStatus::Completed
                && run.event_sequence == 1
                && run.result.as_deref() == Some(event.as_slice()))
            || observed.receipt.commit_sequence < retried.receipt.commit_sequence
        {
            return Err(Error::Control(
                "public qualification Workflow retry state differs",
            ));
        }
        Ok(())
    }
}
